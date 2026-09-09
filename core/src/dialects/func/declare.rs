use crate::attributes::AttributeValue;
use crate::builtin::FnType;
use crate::symbol_table::visibility_of;
use crate::{
    Callable, Context, Error, IRFormatter, Operation, Symbol, TypeId, Visibility, operation,
};

use crate as tir;

// Declares a function another module defines: a λ node with no body, producing
// the same `!fn` value a definition would.
operation! {
    DeclareOp {
        name: "declare",
        dialect: "func",
        format: "custom",
        interfaces: [Symbol, Callable],
        attributes: A {
            sym_name: "Str",
        },
        results: R {
            result: "crate::builtin::FnType",
        },
    }
}

impl Symbol for DeclareOp {
    fn symbol_name(&self) -> String {
        self.sym_name()
    }

    fn symbol_signature(&self) -> Option<Vec<TypeId>> {
        self.signature().map(|(params, _)| params)
    }

    fn symbol_result_type(&self) -> Option<TypeId> {
        self.signature().map(|(_, ret)| ret)
    }

    fn symbol_visibility(&self) -> Visibility {
        visibility_of(self)
    }

    fn is_definition(&self) -> bool {
        false
    }
}

impl Callable for DeclareOp {
    fn body(&self) -> Option<tir::RegionId> {
        None
    }

    fn value(&self) -> tir::ValueId {
        self.fn_value()
    }

    fn params(&self) -> Vec<TypeId> {
        self.signature()
            .expect("a declaration's result is a function type")
            .0
    }

    fn result(&self) -> TypeId {
        self.signature()
            .expect("a declaration's result is a function type")
            .1
    }
}

impl DeclareOp {
    /// The λ value this declaration produces: what a call to it takes as callee.
    pub fn fn_value(&self) -> tir::ValueId {
        self.result()
    }

    fn signature(&self) -> Option<(Vec<TypeId>, TypeId)> {
        FnType::signature_of(&self.0.context.clone(), self.fn_value())
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.clone();
        let visibility = match self.symbol_visibility() {
            Visibility::Private => " private",
            Visibility::Public => "",
        };
        fmt.write(format!(
            "%{} = func.declare{visibility} @{}",
            self.fn_value().number(),
            self.sym_name()
        ))?;
        let (arg_types, ret_type) = self
            .signature()
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

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
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

        let mut builder = DeclareOpBuilder::new(context)
            .attr("sym_name", AttributeValue::Str(sym_name.into()))
            .result_type(FnType::new(context, &arg_types, ret_type));
        if is_private {
            builder = builder.attr(
                "sym_visibility",
                AttributeValue::Str("private".to_string().into()),
            );
        }

        Ok(Box::new(builder.build()))
    }
}

pub fn declare_op(context: &Context, name: &str, ret: TypeId, args: &[TypeId]) -> DeclareOp {
    DeclareOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string().into()))
        .result_type(FnType::new(context, args, ret))
        .build()
}
