use crate::attributes::AttributeValue;
use crate::builtin::FnType;
use crate::symbol_table::visibility_of;
use crate::{Callable, Context, Error, IRFormatter, Symbol, TypeId, Visibility};

use crate as tir;

// Declares a function another module defines: a λ node with no body, producing
// the same `!fn` value a definition would.
tir::helpers::op_def! {
    file: "defs/converted/dialects_func_declare.cdsl",
    hosts: {
        declare_iface_Symbol => (Symbol, &crate::func::__TIR_IFACE_DeclareOp_Symbol),
        declare_iface_Callable => (Callable, &crate::func::__TIR_IFACE_DeclareOp_Callable),
        declare_printer => crate::func::declare_custom_print,
        declare_parser => |parser, context, _definition| crate::func::declare_custom_parse(parser, context),
    },
}

pub(crate) fn declare_symbol_symbol_name(operation: &tir::OpHandle) -> String {
    crate::func::declare_sym_name(operation)
}

pub(crate) fn declare_symbol_symbol_signature(operation: &tir::OpHandle) -> Option<Vec<TypeId>> {
    crate::func::declare_signature(operation).map(|(params, _)| params)
}

pub(crate) fn declare_symbol_symbol_result_type(operation: &tir::OpHandle) -> Option<TypeId> {
    crate::func::declare_signature(operation).map(|(_, ret)| ret)
}

pub(crate) fn declare_symbol_symbol_visibility(operation: &tir::OpHandle) -> Visibility {
    visibility_of(operation)
}

pub(crate) fn declare_symbol_is_definition(__operation: &tir::OpHandle) -> bool {
    false
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_DeclareOp_Symbol: crate::SymbolCallbacks = crate::SymbolCallbacks {
    symbol_name: crate::func::declare_symbol_symbol_name,
    symbol_signature: crate::func::declare_symbol_symbol_signature,
    symbol_result_type: crate::func::declare_symbol_symbol_result_type,
    symbol_visibility: crate::func::declare_symbol_symbol_visibility,
    is_definition: crate::func::declare_symbol_is_definition,
};

pub(crate) fn declare_callable_body(__operation: &tir::OpHandle) -> Option<tir::RegionId> {
    None
}

pub(crate) fn declare_callable_value(operation: &tir::OpHandle) -> tir::ValueId {
    crate::func::declare_fn_value(operation)
}

pub(crate) fn declare_callable_params(operation: &tir::OpHandle) -> Vec<TypeId> {
    crate::func::declare_signature(operation)
        .expect("a declaration's result is a function type")
        .0
}

pub(crate) fn declare_callable_result(operation: &tir::OpHandle) -> TypeId {
    crate::func::declare_signature(operation)
        .expect("a declaration's result is a function type")
        .1
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_DeclareOp_Callable: crate::CallableCallbacks =
    crate::CallableCallbacks {
        body: crate::func::declare_callable_body,
        value: crate::func::declare_callable_value,
        params: crate::func::declare_callable_params,
        result: crate::func::declare_callable_result,
    };

/// The λ value this declaration produces: what a call to it takes as callee.
pub fn declare_fn_value(operation: &tir::OpHandle) -> tir::ValueId {
    crate::func::declare_result(operation)
}

pub(crate) fn declare_signature(operation: &tir::OpHandle) -> Option<(Vec<TypeId>, TypeId)> {
    FnType::signature_of(&operation.context, crate::func::declare_fn_value(operation))
}

pub(crate) fn declare_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    let visibility = match crate::func::declare_symbol_symbol_visibility(operation) {
        Visibility::Private => " private",
        Visibility::Public => "",
    };
    fmt.write(format!(
        "%{} = func.declare{visibility} @{}",
        crate::func::declare_fn_value(operation).number(),
        crate::func::declare_sym_name(operation)
    ))?;
    let (arg_types, ret_type) = crate::func::declare_signature(operation)
        .expect("a declaration's result is a function type");
    fmt.write("(")?;
    for (idx, ty) in arg_types.iter().enumerate() {
        if idx > 0 {
            fmt.write(", ")?;
        }
        context.print_type(*ty, fmt)?;
    }
    fmt.write(") -> ")?;
    context.print_type(ret_type, fmt)?;
    fmt.write("\n")
}

pub(crate) fn declare_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;

    let is_private = parser.parse_token("private");
    let sym_name = parser
        .parse_symbol_name()
        .ok_or_else(|| (parser.span(), Error::ExpectedSymbolName))?
        .to_string();
    let arg_types = parser
        .parse_delimited("(", ")", |parser| {
            parser
                .parse_type(context)?
                .ok_or_else(|| (parser.span(), Error::ExpectedType))
        })?
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("(")))?;

    if !parser.parse_token("->") {
        return Err((parser.span(), Error::ExpectedToken("->")));
    }
    let ret_type = parser
        .parse_type(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedType))?;

    let attributes = if is_private {
        vec![context.named_attribute(
            "sym_visibility",
            AttributeValue::Str("private".to_string().into()),
        )]
    } else {
        vec![]
    };
    Ok(r#declare(
        context,
        AttributeValue::Str(sym_name.into()),
        FnType::new(context, &arg_types, ret_type),
        tir::NewOpParts { attributes },
        tir::InsertionPoint::Root,
    ))
}

pub fn declare_op(
    context: &Context,
    name: &str,
    ret: TypeId,
    args: &[TypeId],
    parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> crate::OpHandle {
    r#declare(
        context,
        AttributeValue::Str(name.to_string().into()),
        FnType::new(context, args, ret),
        parts,
        insertion,
    )
}
