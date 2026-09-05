use crate::Any;
use crate::attributes::AttributeValue;
use crate::builtin::{FnType, UnitType};
use crate::{Context, Error, Operation, ValueId, operation};

use crate as tir;

operation! {
    CallOp {
        name: "call",
        dialect: "func",
        format: "custom",
        verifier: "true",
        operands: O {
            callee: "crate::builtin::FnType",
            args: "*Any",
        },
        results: R {
            result: "Any",
        },
        state: "in_out",
    }
}

impl tir::Verifiable for CallOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let args = self.args();
        super::verify_argument_alignments(self, args.len(), "call")?;
        verify_result_address(self, context, &args)?;

        let Some((parameters, ret_type)) = FnType::signature_of(context, self.callee()) else {
            return Err(Error::VerificationError(
                "call callee must be a function value".to_string(),
            ));
        };
        let arg_types: Vec<_> = args
            .iter()
            .map(|arg| context.get_value(*arg).ty())
            .collect();
        if !tir::symbol_table::signature_accepts(context, &parameters, &arg_types) {
            return Err(Error::VerificationError(format!(
                "call passes ({}), but the callee takes ({})",
                type_list(context, &arg_types),
                type_list(context, &parameters)
            )));
        }
        let result_type = context.get_value(self.result()).ty();
        if result_type != ret_type {
            return Err(Error::VerificationError(format!(
                "call produces {}, but the callee returns {}",
                context.type_to_string(result_type),
                context.type_to_string(ret_type)
            )));
        }
        Ok(())
    }
}

impl CallOp {
    pub fn callee(&self) -> ValueId {
        self.operands()[0]
    }

    pub fn args(&self) -> Vec<ValueId> {
        self.value_operands()[1..].to_vec()
    }

    pub fn has_result_address(&self) -> bool {
        has_result_address(self)
    }

    pub fn argument_alignments(&self) -> Vec<u64> {
        super::argument_alignments(self)
    }

    /// The symbol this call was bound to for machine lowering, once the callee
    /// has been resolved to a λ of the module.
    pub fn callee_symbol(&self) -> Option<String> {
        match self.attr("callee")? {
            AttributeValue::Str(name) => Some(name.to_string()),
            _ => None,
        }
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let ret_type = context.get_value(self.result()).ty();
        let is_unit = ret_type == UnitType::new(&context);

        // A unit result is not spelled, so the binding is written by hand:
        // the value, the dependencies, and `=` only where something is bound.
        let published = self.0.dep_results();
        if !is_unit {
            fmt.write(format!("%{}", self.result().number()))?;
        }
        tir::dependency::print_dep_list(fmt, &published, !is_unit)?;
        if !is_unit || !published.is_empty() {
            fmt.write(" = ")?;
        }
        fmt.write(format!("func.call %{}", self.callee().number()))?;

        let args = self.args();
        fmt.write("(")?;
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                fmt.write(", ")?;
            }
            fmt.write(format!("%{}", arg.number()))?;
        }
        if !args.is_empty() {
            fmt.write(" : ")?;
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    fmt.write(", ")?;
                }
                context.print_type(context.get_value(*arg).ty(), fmt)?;
            }
        }
        fmt.write(")")?;

        if !is_unit {
            fmt.write(" -> ")?;
            context.print_type(ret_type, fmt)?;
        }
        if self.has_result_address() {
            fmt.write(" result_address")?;
        }
        if let Some(symbol) = self.callee_symbol() {
            fmt.write(format!(" callee @{symbol}"))?;
        }
        super::print_argument_alignments(fmt, &self.argument_alignments())?;
        tir::dependency::print_dep_operands(fmt, &self.0)?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        use tir::parse::common::Cursor;
        let callee_ref = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
            .to_string();
        let callee = parser.resolve_value(context, &callee_ref);
        let args = parse_arg_list(parser, context)?;
        let ret_type = parse_ret_type(parser, context)?;
        let result_address = parser.parse_token("result_address");
        let callee_symbol = parser
            .parse_token("callee")
            .then(|| {
                parser
                    .parse_symbol_name()
                    .map(str::to_string)
                    .ok_or_else(|| (parser.span(), Error::ExpectedSymbolName))
            })
            .transpose()?;
        let argument_alignments = super::parse_argument_alignments(parser, context)?;

        let mut builder = CallOpBuilder::new(context)
            .callee(callee)
            .args(args)
            .result_type(ret_type);
        for dep in tir::dependency::parse_dep_operands(parser, context)? {
            builder = builder.dep_operand(dep);
        }
        if result_address {
            builder = builder.result_address();
        }
        if let Some(symbol) = callee_symbol {
            builder = builder.attr("callee", AttributeValue::Str(symbol.into()));
        }
        if let Some(argument_alignments) = argument_alignments {
            builder = builder.attr("argument_alignments", argument_alignments);
        }
        Ok(Box::new(builder.build()))
    }
}

impl CallOpBuilder {
    pub fn result_address(self) -> Self {
        self.attr("result_address", AttributeValue::Bool(true))
    }

    pub fn argument_alignments(self, alignments: &[u64]) -> Self {
        self.attr(
            "argument_alignments",
            AttributeValue::Array(
                alignments
                    .iter()
                    .copied()
                    .map(AttributeValue::UInt)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
    }
}

fn type_list(context: &Context, types: &[tir::TypeId]) -> String {
    types
        .iter()
        .map(|ty| context.type_to_string(*ty))
        .collect::<Vec<_>>()
        .join(", ")
}

fn verify_result_address(
    op: &impl Operation,
    context: &Context,
    args: &[ValueId],
) -> Result<(), Error> {
    if !has_result_address(op) {
        return Ok(());
    }
    let Some(destination) = args.first().copied() else {
        return Err(Error::VerificationError(
            "result-address call requires a destination argument".to_string(),
        ));
    };
    let ty = context.get_type_data(context.get_value(destination).ty());
    if (ty.as_ref() as &dyn std::any::Any)
        .downcast_ref::<crate::ptr::PtrType>()
        .is_none()
    {
        return Err(Error::VerificationError(
            "result-address call destination must have pointer type".to_string(),
        ));
    }
    Ok(())
}

/// Parse `(%a, %b : t1, t2)` (types are informational; values resolve by number).
fn parse_arg_list(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Vec<ValueId>, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    if !parser.parse_token("(") {
        return Err((parser.span(), Error::ExpectedToken("(")));
    }
    let mut args = vec![];
    if parser.parse_token(")") {
        return Ok(args);
    }
    loop {
        let arg_ref = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
            .to_string();
        args.push(parser.resolve_value(context, &arg_ref));
        if !parser.parse_token(",") {
            break;
        }
    }
    if !parser.parse_token(":") {
        return Err((parser.span(), Error::ExpectedToken(":")));
    }
    loop {
        parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        if !parser.parse_token(",") {
            break;
        }
    }
    if !parser.parse_token(")") {
        return Err((parser.span(), Error::ExpectedToken(")")));
    }
    Ok(args)
}

fn parse_ret_type(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::TypeId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    if parser.parse_token("->") {
        parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))
    } else {
        Ok(UnitType::new(context))
    }
}

fn has_result_address(op: &impl Operation) -> bool {
    op.attr("result_address") == Some(AttributeValue::Bool(true))
}
