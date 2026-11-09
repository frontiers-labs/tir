use crate::builtin::UnitType;

use crate::symbol_table::{symbol_name_of, visibility_of};

use crate as tir;
use crate::{Callable, Context, Error, Symbol, Terminator, Visibility};

tir::helpers::op_def! {
    file: "defs/converted/dialects_func_func_op.cdsl",
    hosts: {
        func_verifier => crate::func::func_verify_impl,
        func_iface_Symbol => (Symbol, &crate::func::__TIR_IFACE_FuncOp_Symbol),
        func_iface_Callable => (Callable, &crate::func::__TIR_IFACE_FuncOp_Callable),
        func_printer => crate::func::func_custom_print,
        func_parser => |parser, context, _definition| crate::func::func_custom_parse(parser, context),
        return_iface_Terminator => (Terminator, &crate::TerminatorCallbacks::DEFAULT),
    },
}

/// A λ definition named `name`, its `!fn` type read off the entry block of
/// `body` so the signature can never disagree with the parameters.
pub fn lambda(
    context: &Context,
    name: &str,
    ret_type: tir::TypeId,
    body: &tir::RegionHandle,
    parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> crate::OpHandle {
    let parameters: Vec<_> = body.ports().iter().map(tir::Value::ty).collect();
    r#func(
        context,
        name,
        ret_type,
        tir::builtin::FnType::new(context, &parameters, ret_type),
        Some(body.id()),
        parts,
        insertion,
    )
}

pub(crate) fn func_callable_body(operation: &tir::OpHandle) -> Option<tir::RegionId> {
    Some(crate::func::func_body_region(operation).id())
}

pub(crate) fn func_callable_value(operation: &tir::OpHandle) -> tir::ValueId {
    crate::func::func_fn_value(operation)
}

pub(crate) fn func_callable_params(operation: &tir::OpHandle) -> Vec<tir::TypeId> {
    crate::func::func_parameters(operation)
        .iter()
        .map(tir::Value::ty)
        .collect()
}

pub(crate) fn func_callable_result(operation: &tir::OpHandle) -> tir::TypeId {
    crate::func::func_ret_type(operation)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_FuncOp_Callable: crate::CallableCallbacks =
    crate::CallableCallbacks {
        body: crate::func::func_callable_body,
        value: crate::func::func_callable_value,
        params: crate::func::func_callable_params,
        result: crate::func::func_callable_result,
    };

/// The region holding the body, whichever kind it is. [`func_body`]
/// answers with the entry block, which only an ordered body has.
pub fn func_body_region(operation: &tir::OpHandle) -> tir::RegionHandle {
    operation.context.get_region(
        *operation
            .regions()
            .first()
            .expect("a function owns its body"),
    )
}

/// The function's parameters: the body region's arguments.
pub fn func_parameters(operation: &tir::OpHandle) -> Vec<tir::Value> {
    crate::func::func_body_region(operation).value_arguments()
}

/// The λ value this definition produces: what a call to it takes as callee.
pub fn func_fn_value(operation: &tir::OpHandle) -> tir::ValueId {
    crate::func::func_result(operation)
}

pub fn func_has_result_address(operation: &tir::OpHandle) -> bool {
    operation.attr("result_address") == Some(tir::attributes::AttributeValue::Bool(true))
}

pub fn func_argument_alignments(operation: &tir::OpHandle) -> Vec<u64> {
    super::argument_alignments(operation)
}

/// The parameters the caller guarantees name memory nothing else the
/// function reaches names: a `restrict`-qualified pointer, by index.
pub fn func_noalias_arguments(operation: &tir::OpHandle) -> Vec<usize> {
    super::noalias_arguments(operation)
}

pub(crate) fn func_symbol_symbol_name(operation: &tir::OpHandle) -> String {
    symbol_name_of(operation)
}

pub(crate) fn func_symbol_symbol_signature(operation: &tir::OpHandle) -> Option<Vec<tir::TypeId>> {
    Some(
        crate::func::func_parameters(operation)
            .iter()
            .map(tir::Value::ty)
            .collect(),
    )
}

pub(crate) fn func_symbol_symbol_result_type(operation: &tir::OpHandle) -> Option<tir::TypeId> {
    Some(crate::func::func_ret_type(operation))
}

pub(crate) fn func_symbol_symbol_visibility(operation: &tir::OpHandle) -> Visibility {
    visibility_of(operation)
}

pub(crate) fn func_symbol_is_definition(_operation: &tir::OpHandle) -> bool {
    true
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_FuncOp_Symbol: crate::SymbolCallbacks = crate::SymbolCallbacks {
    symbol_name: crate::func::func_symbol_symbol_name,
    symbol_signature: crate::func::func_symbol_symbol_signature,
    symbol_result_type: crate::func::func_symbol_symbol_result_type,
    symbol_visibility: crate::func::func_symbol_symbol_visibility,
    is_definition: crate::func::func_symbol_is_definition,
};

pub(crate) fn func_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    fmt.write(format!(
        "%{} = func.func",
        crate::func::func_fn_value(operation).number()
    ))?;
    if crate::func::func_symbol_symbol_visibility(operation) == Visibility::Private {
        fmt.write(" private")?;
    }

    let sym_name = match operation.attr("sym_name") {
        Some(tir::attributes::AttributeValue::Str(s)) => s.to_string(),
        Some(_) => panic!("sym_name must be a string"),
        None => "unknown".to_string(),
    };

    fmt.write(format!(" @{}", sym_name))?;

    let context = operation.context.clone();
    let args = crate::func::func_parameters(operation);

    fmt.write("(")?;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{}: ", arg.id().number()))?;
        context.print_type(arg.ty(), fmt)?;
    }
    fmt.write(")")?;

    let ret_type = crate::func::func_ret_type(operation);

    if ret_type != UnitType::new(&context) {
        fmt.write(" -> ")?;
        context.print_type(ret_type, fmt)?;
    }
    if crate::func::func_has_result_address(operation) {
        fmt.write(" result_address")?;
    }
    super::print_keyed_list(
        fmt,
        "argument_alignments",
        &crate::func::func_argument_alignments(operation),
    )?;
    super::print_keyed_list(
        fmt,
        "noalias",
        &crate::func::func_noalias_arguments(operation),
    )?;

    tir::region_format::print_op_region(fmt, &context, operation, 0)?;

    Ok(())
}

pub(crate) fn func_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &tir::Context,
) -> Result<tir::OpHandle, (tir::parse::Span, tir::Error)> {
    // The function's rows share chunks with no other function's.

    use tir::parse::common::Cursor;

    let is_private = parser.parse_token("private");

    let sym_name = parser
        .parse_symbol_name()
        .ok_or_else(|| (parser.span(), tir::Error::ExpectedSymbolName))?
        .to_string();

    let block_args = parser
        .parse_delimited("(", ")", |parser| {
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
            Ok(value)
        })?
        .ok_or_else(|| (parser.span(), tir::Error::ExpectedToken("(")))?;

    let ret_type = if parser.parse_token("->") {
        parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?
    } else {
        UnitType::new(context)
    };
    let result_address = parser.parse_token("result_address");
    let argument_alignments =
        super::parse_keyed_array(parser, context, "argument_alignments", "alignment list")?;
    let noalias = super::parse_keyed_array(parser, context, "noalias", "argument list")?;

    let parameters: Vec<tir::TypeId> = block_args.iter().map(tir::Value::ty).collect();
    let body_region = parser.parse_region_with_entry_args(context, block_args)?;

    let mut parts = tir::NewOpParts::default();
    if result_address {
        parts.attributes.push(context.named_attribute(
            "result_address",
            tir::attributes::AttributeValue::Bool(true),
        ));
    }
    if let Some(argument_alignments) = argument_alignments {
        parts
            .attributes
            .push(context.named_attribute("argument_alignments", argument_alignments));
    }
    if let Some(noalias) = noalias {
        parts
            .attributes
            .push(context.named_attribute("noalias", noalias));
    }
    if is_private {
        parts.attributes.push(context.named_attribute(
            "sym_visibility",
            tir::attributes::AttributeValue::Str("private".to_string().into()),
        ));
    }

    Ok(r#func(
        context,
        sym_name.as_str(),
        ret_type,
        tir::builtin::FnType::new(context, &parameters, ret_type),
        Some(body_region.id()),
        parts,
        tir::InsertionPoint::Root,
    ))
}

pub(crate) fn func_verify_nodes_results(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    let body = crate::func::func_body_region(operation);
    if !body.is_nodes() {
        return Ok(());
    }
    let produced: Vec<tir::TypeId> = body
        .value_results()
        .iter()
        .map(|result| context.get_value(*result).ty())
        .filter(|&ty| !crate::son::CONTROL.is(context, ty))
        .collect();
    let declared = match crate::func::func_ret_type(operation) {
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
        crate::func::func_symbol_symbol_name(operation),
        spell(&produced),
        spell(&declared)
    )))
}

pub(crate) fn func_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    super::verify_argument_alignments(
        operation,
        crate::func::func_parameters(operation).len(),
        "function",
    )?;
    let parameters: Vec<_> = crate::func::func_parameters(operation)
        .iter()
        .map(tir::Value::ty)
        .collect();
    super::verify_noalias_arguments(context, operation, &parameters)?;
    let expected =
        tir::builtin::FnType::new(context, &parameters, crate::func::func_ret_type(operation));
    if context
        .get_value(crate::func::func_fn_value(operation))
        .ty()
        != expected
    {
        return Err(Error::VerificationError(format!(
            "function '@{}' produces {}, but its signature is {}",
            crate::func::func_symbol_symbol_name(operation),
            context.type_to_string(
                context
                    .get_value(crate::func::func_fn_value(operation))
                    .ty()
            ),
            context.type_to_string(expected)
        )));
    }
    crate::func::func_verify_nodes_results(operation, context)?;
    if !crate::func::func_has_result_address(operation) {
        return Ok(());
    }
    let Some(argument) = crate::func::func_parameters(operation).first().cloned() else {
        return Err(Error::VerificationError(
            "result-address function requires a destination argument".to_string(),
        ));
    };
    if crate::ptr::PtrType::view(context, argument.ty()).is_none() {
        return Err(Error::VerificationError(
            "result-address function destination must have pointer type".to_string(),
        ));
    }
    Ok(())
}

/// The value the function returns, or `None` for a void return. The
/// state operand names the memory handed back to the caller, not a
/// value.
pub fn return_returned_value(operation: &tir::OpHandle) -> Option<crate::ValueId> {
    operation.value_operands().first().copied()
}
