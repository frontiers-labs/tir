use crate::attributes::AttributeValue;
use crate::symbol_table::visibility_of;
use crate::{Context, Error, IRFormatter, Operation, Symbol, TypeId, Visibility, operation};

use crate as tir;

operation! {
    DeclareOp {
        name: "declare",
        dialect: "builtin",
        format: "custom",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            ret_type: "Type",
            arg_types: "Array",
        },
    }
}

impl Symbol for DeclareOp {
    fn symbol_name(&self) -> String {
        self.sym_name()
    }

    fn symbol_signature(&self) -> Option<Vec<TypeId>> {
        Some(self.arg_types())
    }

    fn symbol_result_type(&self) -> Option<TypeId> {
        Some(self.ret_type())
    }

    fn symbol_visibility(&self) -> Visibility {
        visibility_of(self)
    }

    fn is_definition(&self) -> bool {
        false
    }
}

impl DeclareOp {
    pub fn sym_name(&self) -> String {
        self.attributes()
            .iter()
            .find(|attr| attr.name == "sym_name")
            .and_then(|attr| match &attr.value {
                AttributeValue::Str(name) => Some(name.clone()),
                _ => None,
            })
            .expect("declare must carry sym_name")
    }

    pub fn ret_type(&self) -> TypeId {
        self.attributes()
            .iter()
            .find(|attr| attr.name == "ret_type")
            .and_then(|attr| match attr.value {
                AttributeValue::Type(ty) => Some(ty),
                _ => None,
            })
            .expect("declare must carry ret_type")
    }

    pub fn arg_types(&self) -> Vec<TypeId> {
        self.attributes()
            .iter()
            .find(|attr| attr.name == "arg_types")
            .and_then(|attr| match &attr.value {
                AttributeValue::Array(items) => Some(
                    items
                        .iter()
                        .map(|item| match item {
                            AttributeValue::Type(ty) => *ty,
                            _ => panic!("declare arg_types must contain only types"),
                        })
                        .collect(),
                ),
                _ => None,
            })
            .expect("declare must carry arg_types")
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let visibility = match self.symbol_visibility() {
            Visibility::Private => " private",
            Visibility::Public => "",
        };
        fmt.write(format!("declare{visibility} @{}(", self.sym_name()))?;
        for (idx, ty) in self.arg_types().iter().enumerate() {
            if idx > 0 {
                fmt.write(", ")?;
            }
            context.print_type(*ty, fmt)?;
        }
        fmt.write(") -> ")?;
        context.print_type(self.ret_type(), fmt)?;
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
        if !parser.parse_token("(") {
            return Err((parser.span(), Error::ExpectedToken("(")));
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

        let mut builder = DeclareOpBuilder::new(context)
            .attr("sym_name", AttributeValue::Str(sym_name))
            .attr("ret_type", AttributeValue::Type(ret_type))
            .attr("arg_types", arg_types_attr(&arg_types));
        if is_private {
            builder = builder.attr("sym_visibility", AttributeValue::Str("private".to_string()));
        }

        Ok(Box::new(builder.build()))
    }
}

operation! {
    AddressOfOp {
        name: "addr_of",
        dialect: "builtin",
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
        self.attributes()
            .iter()
            .find(|attr| attr.name == "sym_name")
            .and_then(|attr| match &attr.value {
                AttributeValue::Str(name) => Some(name.clone()),
                _ => None,
            })
            .expect("addr_of must carry sym_name")
    }
}

pub fn addr_of_op(context: &Context, name: &str, result_type: TypeId) -> AddressOfOp {
    AddressOfOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string()))
        .result_type(result_type)
        .build()
}

pub fn arg_types_attr(types: &[TypeId]) -> AttributeValue {
    AttributeValue::Array(types.iter().copied().map(AttributeValue::Type).collect())
}

pub fn declare_op(context: &Context, name: &str, ret: TypeId, args: &[TypeId]) -> DeclareOp {
    DeclareOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string()))
        .attr("ret_type", AttributeValue::Type(ret))
        .attr("arg_types", arg_types_attr(args))
        .build()
}
