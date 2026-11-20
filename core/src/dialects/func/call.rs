use crate::attributes::AttributeValue;
use crate::builtin::{StateResource, UnitType};
use crate::{
    Apply, Context, Error, Operation, ResourceAccess, ResourceEffect, ResourceEffects, ValueId,
};

use crate as tir;

__cdsl_func_CallOp!();

impl ResourceEffects for CallOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        vec![ResourceEffect {
            resource: StateResource::Memory,
            access: ResourceAccess::Change,
            observed: self.0.state_operands().into_vec(),
            produced: self.0.state_results().into_vec(),
        }]
    }
}

impl Apply for CallOp {
    fn callee(&self) -> ValueId {
        CallOp::callee(self)
    }

    fn args(&self) -> std::ops::Range<usize> {
        1..self.value_operands().len()
    }
}

impl CallOp {
    /// Resources the callee may access. An omitted summary is conservative.
    pub fn resources(&self) -> Result<Vec<StateResource>, Error> {
        let Some(value) = self.attr("resources") else {
            return Ok(vec![StateResource::Memory]);
        };
        if let AttributeValue::Array(values) = value
            && let [AttributeValue::Str(name)] = values.as_ref()
            && name.as_ref() == StateResource::Memory.name()
        {
            return Ok(vec![StateResource::Memory]);
        }
        Err(Error::VerificationError(
            "call resources must be a non-empty list of distinct resource names".into(),
        ))
    }

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
        let context = self.0.context.clone();
        let ret_type = context.get_value(self.result()).ty();
        let is_unit = ret_type == UnitType::new(&context);

        // A unit result is not spelled, so the binding is written by hand:
        // the value, the states, and `=` only where something is bound.
        let published = self.0.state_results();
        if !is_unit {
            fmt.write(format!("%{}", self.result().number()))?;
        }
        if !is_unit && !published.is_empty() {
            fmt.write(", ")?;
        }
        tir::region_format::print_value_list(fmt, &published)?;
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
        super::print_keyed_list(fmt, "argument_alignments", &self.argument_alignments())?;
        if self.attr("resources").is_some() {
            let resources = self.resources().map_err(|_| std::fmt::Error)?;
            let names = resources
                .iter()
                .map(|resource| format!("\"{}\"", resource.name()))
                .collect::<Vec<_>>();
            fmt.write(format!(" resources [{}]", names.join(", ")))?;
        }
        tir::region_format::print_state_operands(fmt, &self.0)?;
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
        let argument_alignments =
            super::parse_keyed_array(parser, context, "argument_alignments", "alignment list")?;

        let resources = super::parse_keyed_array(parser, context, "resources", "resource list")?;
        let mut builder = CallOpBuilder::new(context)
            .callee(callee)
            .args(args)
            .result_type(ret_type);
        builder = builder.state_operands(parser.parse_state_operands(context)?);
        if result_address {
            builder = builder.result_address();
        }
        if let Some(symbol) = callee_symbol {
            builder = builder.attr("callee", AttributeValue::Str(symbol.into()));
        }
        if let Some(argument_alignments) = argument_alignments {
            builder = builder.attr("argument_alignments", argument_alignments);
        }
        if let Some(resources) = resources {
            builder = builder.attr("resources", resources);
        }
        Ok(Box::new(builder.build()))
    }
}

impl CallOpBuilder {
    /// Record a known callee's effect contract instead of assuming all resources.
    pub fn resources(self, resources: &[StateResource]) -> Self {
        self.attr(
            "resources",
            AttributeValue::Array(
                resources
                    .iter()
                    .map(|resource| AttributeValue::from(resource.name()))
                    .collect(),
            ),
        )
    }

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
