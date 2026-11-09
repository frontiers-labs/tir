use tir::{Context, Error, OpHandle, Terminator, ValueId};

use tir;

tir::helpers::op_def! {
    file: "../core/defs/converted-gpu/spirv_ops.cdsl",
    hosts: {
        module_end_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        AccessChain_printer => access_chain_custom_print,
        AccessChain_parser => |parser, context, _definition| access_chain_custom_parse(parser, context),
        Return_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
    },
}

fn access_chain_custom_print(
    operation: &OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = &operation.context;
    let operands = operation.operands();
    fmt.write(format!(
        "%{} = spirv.AccessChain %{}",
        operation.value_results()[0].number(),
        operands[0].number()
    ))?;
    for index in &operands[1..] {
        fmt.write(format!(", %{}", index.number()))?;
    }
    fmt.write(" : ")?;
    context.print_type(context.get_value(operation.value_results()[0]).ty(), fmt)?;
    fmt.write("\n")
}

fn access_chain_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<OpHandle, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let base = parse_value(parser, context)?;
    let mut indices = Vec::new();
    while parser.parse_token(",") {
        indices.push(parse_value(parser, context)?);
    }
    if !parser.parse_token(":") {
        return Err((parser.span(), Error::ExpectedToken(":")));
    }
    let result_type = parser
        .parse_type(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
    Ok(r#AccessChain(
        context,
        base,
        indices,
        result_type,
        Default::default(),
        tir::InsertionPoint::Root,
    ))
}

fn parse_value(
    parser: &mut tir::parse::text::Parser<'_>,
    context: &Context,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    Ok(parser.resolve_value(context, name))
}
