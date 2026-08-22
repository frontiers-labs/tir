use std::any::Any;
use std::sync::Arc;

use tir::parse::common::Cursor;
use tir::{
    Context, Error, IRFormatter, TirType, Type, TypeConstraint, TypeId, dialect, operation,
    parse::Span,
};

pub mod ops {
    pub use super::{
        CopyStructOp, DefineStructOp, GetMemberOp, VaArgOp, VaEndOp, VaStartOp, copy_struct,
        define_struct, get_member, va_arg, va_end, va_start,
    };
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
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
