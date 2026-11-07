use crate::attributes::AttributeValue;
use crate::builtin::{FnType, StateResource, UnitType};
use crate::{
    Apply, Context, Error, Operation, ResourceAccess, ResourceEffect, ResourceEffects, ValueId,
    operation,
};

use crate as tir;

operation! {
    CallOp {
        name: "call",
        dialect: "func",
        scheme: "(fn<_, _>, _*) -> _",
        format: "custom",
        verifier: "true",
        operands: O { callee, args },
        results: R { result },
        state: "in_out",
        interfaces: [Apply, ResourceEffects],
    }
}

impl ResourceEffects for CallOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        self.resources()
            .unwrap_or_else(|_| vec![StateResource::Memory, StateResource::FpEnv])
            .into_iter()
            .map(|resource| ResourceEffect {
                resource,
                access: ResourceAccess::Change,
                observed: self.0.context.states_of(self.0.state_operands(), resource),
                produced: self.0.context.states_of(self.0.state_results(), resource),
            })
            .collect()
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

impl tir::Verifiable for CallOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        self.resources()?;
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
    /// Resources the callee may access. An omitted summary is conservative.
    pub fn resources(&self) -> Result<Vec<StateResource>, Error> {
        let Some(value) = self.attr("resources") else {
            return Ok(vec![StateResource::Memory, StateResource::FpEnv]);
        };
        let invalid = || {
            Error::VerificationError(
                "call resources must be a non-empty list of distinct resource names".into(),
            )
        };
        let AttributeValue::Array(values) = value else {
            return Err(invalid());
        };
        if values.is_empty() {
            return Err(invalid());
        }
        let mut resources = Vec::new();
        for value in values {
            let AttributeValue::Str(name) = value else {
                return Err(invalid());
            };
            let resource = [StateResource::Memory, StateResource::FpEnv]
                .into_iter()
                .find(|resource| resource.name() == name.as_ref())
                .ok_or_else(invalid)?;
            if resources.contains(&resource) {
                return Err(invalid());
            }
            resources.push(resource);
        }
        Ok(resources)
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
        let state = parser.parse_state_operands(context)?;
        let mut parts = tir::NewOpParts::default();
        if result_address {
            parts
                .attributes
                .push(context.named_attribute("result_address", AttributeValue::Bool(true)));
        }
        if let Some(symbol) = callee_symbol {
            parts
                .attributes
                .push(context.named_attribute("callee", AttributeValue::Str(symbol.into())));
        }
        if let Some(argument_alignments) = argument_alignments {
            parts
                .attributes
                .push(context.named_attribute("argument_alignments", argument_alignments));
        }
        if let Some(resources) = resources {
            parts
                .attributes
                .push(context.named_attribute("resources", resources));
        }
        Ok(Box::new(CallOp::new(
            context,
            callee,
            args,
            ret_type,
            state,
            vec![],
            parts,
            tir::InsertionPoint::Root,
        )))
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
    if crate::ptr::PtrType::view(context, context.get_value(destination).ty()).is_none() {
        return Err(Error::VerificationError(
            "result-address call destination must have pointer type".to_string(),
        ));
    }
    Ok(())
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
