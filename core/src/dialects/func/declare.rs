use crate::attributes::AttributeValue;
use crate::symbol_table::visibility_of;
use crate::{Context, Error, IRFormatter, Operation, Symbol, TypeId, Visibility, operation};

use crate as tir;

// Declares a symbol another module defines: a function, carrying `ret_type`
// and `arg_types`, or a data object, carrying neither.
operation! {
    DeclareOp {
        name: "declare",
        dialect: "func",
        format: "custom",
        verifier: "true",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
        },
    }
}

impl Symbol for DeclareOp {
    fn symbol_name(&self) -> String {
        self.sym_name()
    }

    fn symbol_signature(&self) -> Option<Vec<TypeId>> {
        self.arg_types()
    }

    fn symbol_result_type(&self) -> Option<TypeId> {
        self.ret_type()
    }

    fn symbol_visibility(&self) -> Visibility {
        visibility_of(self)
    }

    fn is_definition(&self) -> bool {
        false
    }
}

impl tir::Verifiable for DeclareOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        let name = self.sym_name();
        let carries = |attr_name: &str| self.attr(attr_name).is_some();
        match (carries("ret_type"), carries("arg_types")) {
            (false, false) => Ok(()),
            (true, true) => {
                if self.ret_type().is_none() {
                    return Err(Error::VerificationError(format!(
                        "declare '@{name}' attribute 'ret_type' must be a type"
                    )));
                }
                if self.arg_types().is_none() {
                    return Err(Error::VerificationError(format!(
                        "declare '@{name}' attribute 'arg_types' must be an array of types"
                    )));
                }
                Ok(())
            }
            (ret, _) => {
                let (present, missing) = if ret {
                    ("ret_type", "arg_types")
                } else {
                    ("arg_types", "ret_type")
                };
                Err(Error::VerificationError(format!(
                    "declare '@{name}' carries '{present}' without '{missing}': a function \
                     declaration carries both, a data declaration neither"
                )))
            }
        }
    }
}

impl DeclareOp {
    pub fn sym_name(&self) -> String {
        match self.attr("sym_name") {
            Some(AttributeValue::Str(name)) => name.to_string(),
            _ => panic!("declare must carry sym_name"),
        }
    }

    /// `None` for a data declaration, which has no signature.
    pub fn ret_type(&self) -> Option<TypeId> {
        match self.attr("ret_type")? {
            AttributeValue::Type(ty) => Some(ty),
            _ => None,
        }
    }

    /// `None` for a data declaration, which has no signature.
    pub fn arg_types(&self) -> Option<Vec<TypeId>> {
        match self.attr("arg_types")? {
            AttributeValue::Array(items) => items
                .iter()
                .map(|item| match item {
                    AttributeValue::Type(ty) => Some(*ty),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let visibility = match self.symbol_visibility() {
            Visibility::Private => " private",
            Visibility::Public => "",
        };
        fmt.write(format!("func.declare{visibility} @{}", self.sym_name()))?;
        let (Some(arg_types), Some(ret_type)) = (self.arg_types(), self.ret_type()) else {
            return fmt.write("\n");
        };
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
        let mut builder =
            DeclareOpBuilder::new(context).attr("sym_name", AttributeValue::Str(sym_name.into()));
        if is_private {
            builder = builder.attr(
                "sym_visibility",
                AttributeValue::Str("private".to_string().into()),
            );
        }
        if !parser.parse_token("(") {
            return Ok(Box::new(builder.build()));
        }

        let mut arg_types = Vec::new();
        if !parser.parse_token(")") {
            loop {
                let ty = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                arg_types.push(ty);

                if parser.parse_token(")") {
                    break;
                }
                if !parser.parse_token(",") {
                    return Err((parser.span(), Error::ExpectedToken(",")));
                }
            }
        }

        if !parser.parse_token("->") {
            return Err((parser.span(), Error::ExpectedToken("->")));
        }
        let ret_type = parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;

        let builder = builder
            .attr("ret_type", AttributeValue::Type(ret_type))
            .attr("arg_types", arg_types_attr(&arg_types));

        Ok(Box::new(builder.build()))
    }
}

operation! {
    AddressOfOp {
        name: "addr_of",
        dialect: "func",
        verifier: "true",
        attributes: A {
            sym_name: "Str",
        },
        results: R {
            result: "crate::ptr::PtrType",
        },
    }
}

impl tir::Verifiable for AddressOfOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        tir::symbol_table::resolve_symbol_use(context, self.id(), &self.sym_name(), None)?;
        Ok(())
    }
}

impl AddressOfOp {
    pub fn sym_name(&self) -> String {
        match self.attr("sym_name") {
            Some(AttributeValue::Str(name)) => name.to_string(),
            _ => panic!("addr_of must carry sym_name"),
        }
    }
}

pub fn addr_of_op(context: &Context, name: &str, result_type: TypeId) -> AddressOfOp {
    AddressOfOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string().into()))
        .result_type(result_type)
        .build()
}

pub fn arg_types_attr(types: &[TypeId]) -> AttributeValue {
    AttributeValue::Array(
        types
            .iter()
            .copied()
            .map(AttributeValue::Type)
            .collect::<Vec<_>>()
            .into(),
    )
}

pub fn declare_op(context: &Context, name: &str, ret: TypeId, args: &[TypeId]) -> DeclareOp {
    DeclareOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string().into()))
        .attr("ret_type", AttributeValue::Type(ret))
        .attr("arg_types", arg_types_attr(args))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Operation;
    use crate::builtin::IntegerType;

    /// A half-signature declare is neither a function nor a data declaration,
    /// and the text form cannot express it: only a builder can build one.
    #[test]
    fn declare_carrying_a_return_type_without_arguments_is_rejected() {
        let context = Context::with_default_dialects();
        let declaration = DeclareOpBuilder::new(&context)
            .attr(
                "sym_name",
                AttributeValue::Str("counter".to_string().into()),
            )
            .attr(
                "ret_type",
                AttributeValue::Type(IntegerType::new(&context, 32)),
            )
            .build();

        let error = declaration
            .verify(&context)
            .expect_err("a data declaration must not carry a return type");

        assert!(
            format!("{error:?}").contains("carries 'ret_type' without 'arg_types'"),
            "{error:?}"
        );
    }
}
