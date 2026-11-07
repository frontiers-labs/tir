use crate::ty::{Ctor, Kind, TypeCtor};
use crate::{Context, Error, Operation, dialect, operation};

/// The constructor of an explicit control-flow token.
pub static CONTROL: Ctor = Ctor::new(TypeCtor {
    dialect: "son",
    name: "control",
    params: &[],
    kind: Kind::Data,
    validate: tir_symbolic::term::valid,
    carrier: tir_symbolic::term::no_carrier,
});

operation! {
    MergeOp {
        name: "merge",
        dialect: "son",
        scheme: "(son.control*) -> son.control",
        operands: O { incoming },
        results: R { control },
    }
}

operation! {
    EdgeOp {
        name: "edge",
        dialect: "son",
        scheme: "(son.control, _*) -> son.control",
        operands: O { control, arguments },
        results: R { edge },
        state: "in",
    }
}

operation! {
    BranchOp {
        name: "branch",
        dialect: "son",
        scheme: "(son.control, int<1>) -> (son.control, son.control)",
        format: "custom",
        operands: O { control, predicate },
        results: R { taken, fallthrough },
    }
}

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
        Ok(Box::new(BranchOp::new(
            context,
            *control,
            *predicate,
            vec![control_type; 2],
            Default::default(),
            tir::InsertionPoint::Root,
        )))
    }
}

dialect! {
    SonDialect {
        name: "son",
        operations: [MergeOp, EdgeOp, BranchOp],
        types: [Ctor(&CONTROL)],
    }
}
