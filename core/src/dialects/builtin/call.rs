use crate::Any;
use crate::attributes::AttributeValue;
use crate::builtin::UnitType;
use crate::{Context, Error, Operation, ValueId, operation};

use crate as tir;

operation! {
    CallOp {
        name: "call",
        dialect: "builtin",
        format: "custom",
        operands: O {
            args: "*Any",
        },
        attributes: A {
            callee: "Str",
        },
        results: R {
            result: "Any",
        },
    }
}

impl CallOp {
    pub fn callee(&self) -> String {
        callee_attr(self)
    }

    pub fn args(&self) -> Vec<ValueId> {
        self.operands().to_vec()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let header = format!("call @{}", self.callee());
        print_call(&context, fmt, &header, self.result(), &self.args())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        use tir::parse::common::Cursor;
        let callee = parser
            .parse_symbol_name()
            .ok_or_else(|| (parser.span(), Error::ExpectedSymbolName))?
            .to_string();
        let args = parse_arg_list(parser, context)?;
        let ret_type = parse_ret_type(parser, context)?;

        Ok(Box::new(
            CallOpBuilder::new(context)
                .args(args)
                .attr("callee", AttributeValue::Str(callee))
                .result_type(ret_type)
                .build(),
        ))
    }
}

operation! {
    IndirectCallOp {
        name: "indirect_call",
        dialect: "builtin",
        format: "custom",
        operands: O {
            callee: "Any",
            args: "*Any",
        },
        results: R {
            result: "Any",
        },
    }
}

impl IndirectCallOp {
    pub fn callee(&self) -> ValueId {
        self.operands()[0]
    }

    pub fn args(&self) -> Vec<ValueId> {
        self.operands()[1..].to_vec()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let header = format!("indirect_call %{}", self.callee().number());
        print_call(&context, fmt, &header, self.result(), &self.args())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        use tir::parse::common::Cursor;
        let callee = parser
            .parse_value_ref()
            .and_then(|name| name.parse::<u32>().ok())
            .map(ValueId::from_number)
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
        let args = parse_arg_list(parser, context)?;
        let ret_type = parse_ret_type(parser, context)?;

        Ok(Box::new(
            IndirectCallOpBuilder::new(context)
                .callee(callee)
                .args(args)
                .result_type(ret_type)
                .build(),
        ))
    }
}

/// Print a call as `%r = <header>(%a, %b : t1, t2) -> ret`, omitting the result
/// binding and arrow for unit-returning calls.
fn print_call(
    context: &Context,
    fmt: &mut tir::IRFormatter,
    header: &str,
    result: ValueId,
    args: &[ValueId],
) -> Result<(), std::fmt::Error> {
    let ret_type = context.get_value(result).ty();
    let is_unit = ret_type == UnitType::new(context);

    if !is_unit {
        fmt.write(format!("%{} = ", result.number()))?;
    }
    fmt.write(header)?;

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
    fmt.write("\n")
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
        let arg = parser
            .parse_value_ref()
            .and_then(|name| name.parse::<u32>().ok())
            .map(ValueId::from_number)
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
        args.push(arg);
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

fn callee_attr(op: &impl Operation) -> String {
    op.attributes()
        .iter()
        .find(|a| a.name == "callee")
        .and_then(|a| match &a.value {
            AttributeValue::Str(s) => Some(s.clone()),
            _ => None,
        })
        .expect("call must carry a 'callee' symbol name")
}

#[cfg(test)]
mod tests {
    use crate::{Context, IRBuilder, builtin::IntegerType};

    // Call roundtrip coverage (direct, void, indirect) lives in the FileCheck
    // suite at core/checks/IRRoundtrip/call.tir.

    #[test]
    fn call_construction() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let a = context.create_value(i32_ty, None);
        let b = context.create_value(i32_ty, None);
        let (a_id, b_id) = (a.id(), b.id());
        let _block = context.create_block(vec![a, b]);

        let call = super::CallOpBuilder::new(&context)
            .args(vec![a_id, b_id])
            .attr(
                "callee",
                crate::attributes::AttributeValue::Str("foo".into()),
            )
            .result_type(i32_ty)
            .build();
        assert_eq!(call.callee(), "foo");
        assert_eq!(call.args(), vec![a_id, b_id]);
        assert_eq!(context.get_value(call.result()).ty(), i32_ty);
    }

    #[test]
    fn indirect_call_operand_split() {
        let context = Context::with_default_dialects();
        let i64_ty = IntegerType::new(&context, 64);
        let i32_ty = IntegerType::new(&context, 32);
        let callee = context.create_value(i64_ty, None);
        let arg = context.create_value(i32_ty, None);
        let (callee_id, arg_id) = (callee.id(), arg.id());
        let _block = context.create_block(vec![callee, arg]);

        let call = super::IndirectCallOpBuilder::new(&context)
            .callee(callee_id)
            .args(vec![arg_id])
            .result_type(i32_ty)
            .build();
        assert_eq!(call.callee(), callee_id);
        assert_eq!(call.args(), vec![arg_id]);
    }

    #[test]
    fn call_args_are_used() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let a = context.create_value(i32_ty, None);
        let a_id = a.id();
        let region = context.create_region();
        let block = context.create_block(vec![a]);
        region.add_block(block.id());

        let func = crate::builtin::ops::func(&context, "caller", i32_ty, Some(region.id())).build();
        let mut builder = IRBuilder::new(func.body());
        let call = super::CallOpBuilder::new(&context)
            .args(vec![a_id])
            .attr(
                "callee",
                crate::attributes::AttributeValue::Str("foo".into()),
            )
            .result_type(i32_ty)
            .build();
        let result = call.result();
        builder.insert(call);
        builder.insert(crate::builtin::ops::r#return(&context, result).build());

        assert!(context.is_value_used(a_id));
        assert!(context.is_value_used(result));
    }
}
