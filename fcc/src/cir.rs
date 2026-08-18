use std::any::Any;
use std::sync::Arc;

use tir::attributes::AttributeValue;
use tir::parse::common::Cursor;
use tir::symbol_table::visibility_of;
use tir::{
    Context, Error, IRFormatter, Operation, Symbol, TirType, Type, TypeConstraint, TypeId,
    Visibility, dialect, operation, parse::Span,
};

pub mod ops {
    pub use super::{
        CopyStructOp, DefineStructOp, GetMemberOp, GlobalOp, GlobalStringOp, VaArgOp, VaEndOp,
        VaStartOp, ZeroGlobalOp, copy_struct, define_struct, get_member, global, global_string,
        va_arg, va_end, va_start, zero_global,
    };
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
            GlobalOp,
            GlobalStringOp,
            ZeroGlobalOp,
            DefineStructOp,
            GetMemberOp,
            CopyStructOp,
            VaStartOp,
            VaArgOp,
            VaEndOp,
        ],
        types: [StructType, VarArgsType, VaListType],
    }
}

operation! {
    GlobalOp {
        name: "global",
        dialect: "cir",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            bytes: "Array",
            relocations: "Array",
            align: "UInt",
        },
    }
}

operation! {
    GlobalStringOp {
        name: "global_string",
        dialect: "cir",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            value: "Str",
        },
    }
}

/// A named object: it owns its name outright, so it carries no signature and
/// cannot be overloaded.
macro_rules! data_symbol {
    ($op:ty) => {
        impl Symbol for $op {
            fn symbol_name(&self) -> String {
                self.sym_name()
            }

            fn symbol_signature(&self) -> Option<Vec<TypeId>> {
                None
            }

            fn symbol_result_type(&self) -> Option<TypeId> {
                None
            }

            fn symbol_visibility(&self) -> Visibility {
                visibility_of(self)
            }

            fn is_definition(&self) -> bool {
                true
            }
        }
    };
}

data_symbol!(GlobalOp);
data_symbol!(ZeroGlobalOp);
data_symbol!(GlobalStringOp);

impl GlobalStringOp {
    pub fn sym_name(&self) -> String {
        string_attribute(self, "sym_name")
    }

    pub fn value(&self) -> String {
        string_attribute(self, "value")
    }
}

impl GlobalOp {
    pub fn sym_name(&self) -> String {
        string_attribute(self, "sym_name")
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.attr("bytes")
            .and_then(|value| match value {
                AttributeValue::Array(bytes) => bytes
                    .iter()
                    .map(|byte| match byte {
                        AttributeValue::UInt(value) => u8::try_from(*value).ok(),
                        _ => None,
                    })
                    .collect(),
                _ => None,
            })
            .unwrap()
    }

    pub fn align(&self) -> u64 {
        uint_attribute(self, "align")
    }

    pub fn relocations(&self) -> Vec<(u64, String, i64, u64)> {
        self.attr("relocations")
            .and_then(|value| match value {
                AttributeValue::Array(relocations) => Some(
                    relocations
                        .iter()
                        .map(|relocation| {
                            let AttributeValue::Dict(fields) = relocation else {
                                unreachable!("cir.global relocation is a dictionary")
                            };
                            let AttributeValue::UInt(offset) = fields.get("offset").unwrap() else {
                                unreachable!("cir.global relocation has an offset")
                            };
                            let AttributeValue::Str(symbol) = fields.get("symbol").unwrap() else {
                                unreachable!("cir.global relocation has a symbol")
                            };
                            let AttributeValue::Int(addend) = fields.get("addend").unwrap() else {
                                unreachable!("cir.global relocation has an addend")
                            };
                            let AttributeValue::UInt(width) = fields.get("width").unwrap() else {
                                unreachable!("cir.global relocation has a width")
                            };
                            (*offset, symbol.to_string(), *addend, *width)
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap()
    }
}

operation! {
    ZeroGlobalOp {
        name: "zero_global",
        dialect: "cir",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            size: "UInt",
            align: "UInt",
        },
    }
}

impl ZeroGlobalOp {
    pub fn sym_name(&self) -> String {
        string_attribute(self, "sym_name")
    }

    pub fn size(&self) -> u64 {
        uint_attribute(self, "size")
    }

    pub fn align(&self) -> u64 {
        uint_attribute(self, "align")
    }
}

fn string_attribute(operation: &impl Operation, name: &str) -> String {
    match operation.attr(name) {
        Some(AttributeValue::Str(value)) => value.to_string(),
        _ => panic!("{name} must be a string attribute"),
    }
}

fn uint_attribute(operation: &impl Operation, name: &str) -> u64 {
    operation
        .attr(name)
        .and_then(|value| match value {
            AttributeValue::UInt(value) => Some(value),
            _ => None,
        })
        .unwrap()
}

pub struct StructType {
    name: String,
}

impl StructType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, name: impl Into<String>) -> TypeId {
        context.get_type_id(Arc::new(Self { name: name.into() }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl TypeConstraint for StructType {}

impl Type for StructType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "struct"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        if !parser.parse_token("<") {
            return Err((parser.span(), Error::ExpectedToken("<")));
        }
        let name = parser
            .parse_string()
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("struct name")))?;
        if !parser.parse_token(">") {
            return Err((parser.span(), Error::ExpectedToken(">")));
        }
        Ok(Self::new(context, name))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write(format!("struct<\"{}\">", self.name))
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any)
            .downcast_ref::<StructType>()
            .is_some_and(|other| other.name == self.name)
    }

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        state.write(self.name.as_bytes());
    }
}

operation! {
    DefineStructOp {
        name: "define_struct",
        dialect: "cir",
        attributes: A {
            sym_name: "Str",
            fields: "Array",
            size: "UInt",
            align: "UInt",
        },
    }
}

operation! {
    GetMemberOp {
        name: "get_member",
        dialect: "cir",
        operands: O {
            base: "tir::ptr::PtrType",
        },
        attributes: A {
            field: "UInt",
            struct_name: "Str",
        },
        results: R {
            result: "tir::ptr::PtrType",
        },
    }
}

operation! {
    CopyStructOp {
        name: "copy_struct",
        dialect: "cir",
        operands: O {
            destination: "tir::ptr::PtrType",
            source: "tir::ptr::PtrType",
        },
        attributes: A {
            struct_name: "Str",
        },
    }
}

#[derive(TirType)]
#[tir_type(dialect = "cir", name = "varargs")]
pub struct VarArgsType;

impl VarArgsType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for VarArgsType {}

impl Type for VarArgsType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "varargs"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("varargs")
    }

    fn is_variadic_tail(&self) -> bool {
        true
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<VarArgsType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

#[derive(TirType)]
#[tir_type(dialect = "cir", name = "va_list")]
pub struct VaListType;

impl VaListType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for VaListType {}

impl Type for VaListType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "va_list"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("va_list")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<VaListType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

operation! {
    VaStartOp {
        name: "va_start",
        dialect: "cir",
        results: R {
            result: "crate::cir::VaListType",
        },
    }
}

operation! {
    VaArgOp {
        name: "va_arg",
        dialect: "cir",
        operands: O {
            list: "crate::cir::VaListType",
        },
        results: R {
            result: "tir::Any",
        },
    }
}

operation! {
    VaEndOp {
        name: "va_end",
        dialect: "cir",
        operands: O {
            list: "crate::cir::VaListType",
        },
    }
}
