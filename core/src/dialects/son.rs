use crate::ty::{Ctor, Kind, TypeCtor};
use crate::{Context, Error, Operation, dialect};

use crate as tir;

/// The constructor of an explicit control-flow token.
pub static CONTROL: Ctor = Ctor::new(TypeCtor {
    dialect: "son",
    name: "control",
    params: &[],
    kind: Kind::Data,
    validate: tir_symbolic::term::valid,
    carrier: tir_symbolic::term::no_carrier,
});

__cdsl_son_MergeOp!();
__cdsl_son_EdgeOp!();
__cdsl_son_BranchOp!();

impl BranchOp {
    fn custom_print(&self, fmt: &mut crate::IRFormatter) -> Result<(), std::fmt::Error> {
        crate::region_format::print_result_prefix(fmt, &self.0)?;
        fmt.write("son.branch ")?;
        crate::region_format::print_value_list(fmt, &self.operands())?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut crate::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (crate::parse::Span, Error)> {
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
        Ok(Box::new(
            branch(context, *control, *predicate, control_type, control_type).build(),
        ))
    }
}

dialect! {
    SonDialect {
        name: "son",
        operations: [MergeOp, EdgeOp, BranchOp],
        types: [Ctor(&CONTROL)],
    }
}
