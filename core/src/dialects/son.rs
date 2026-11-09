use crate::ty::Ctor;
use crate::{Context, Error, dialect};

/// The constructor of an explicit control-flow token.
pub static CONTROL: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_son.cdsl", binding: constructors::CONTROL, hosts: { CONTROL_validate => tir_symbolic::term::valid, CONTROL_carrier => tir_symbolic::term::no_carrier } },
);

tir::helpers::op_def! {
    file: "defs/converted/dialects_son.cdsl",
    hosts: {
        branch_printer => crate::son::branch_custom_print,
        branch_parser => |parser, context, _definition| crate::son::branch_custom_parse(parser, context),
    },
}

pub(crate) fn branch_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut crate::IRFormatter,
) -> Result<(), std::fmt::Error> {
    crate::region_format::print_result_prefix(fmt, operation)?;
    fmt.write("son.branch ")?;
    crate::region_format::print_value_list(fmt, &operation.operands())?;
    fmt.write("\n")
}

pub(crate) fn branch_custom_parse(
    parser: &mut crate::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (crate::parse::Span, Error)> {
    use crate::parse::common::Cursor;
    let mut values = Vec::new();
    while let Some(name) = parser.parse_value_ref() {
        values.push(parser.resolve_value(context, name));
        if !parser.parse_token(",") {
            break;
        }
    }
    let [control, predicate] = values.as_slice() else {
        return Err((parser.span(), Error::ExpectedValueRef));
    };
    let control_type = context
        .terms()
        .intern(CONTROL.id(context), vec![])
        .map_err(|error| (parser.span(), Error::VerificationError(error)))?;
    Ok(r#branch(
        context,
        *control,
        *predicate,
        vec![control_type; 2],
        Default::default(),
        tir::InsertionPoint::Root,
    ))
}

dialect! {
    SonDialect {
        name: "son",
        operations: [MERGE, EDGE, BRANCH],
        types: [Ctor(&CONTROL)],
    }
}
