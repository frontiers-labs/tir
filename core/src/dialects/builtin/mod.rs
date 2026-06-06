mod arith;
mod control;
mod func;
mod module;

use std::any::Any;
use std::sync::Arc;

use crate::ty::TypeConstraint;
use crate::{Context, Error, IRFormatter, Type, TypeId, dialect, parse::Span};

use crate as tir;

pub use arith::*;
pub use control::*;
pub use func::*;
pub use module::*;

pub mod ops {
    pub use super::arith::*;
    pub use super::control::*;
    pub use super::func::*;
    pub use super::module::*;
}

dialect! {
    BuiltinDialect {
        name: "builtin",
        operations: [
            ModuleOp,
            ModuleEndOp,
            FuncOp,
            ReturnOp,
            ConstantOp,
            AddIOp,
            SubIOp,
            MulIOp,
            AndIOp,
            OrIOp,
            XOrIOp,
            ShlIOp,
            ShrUIOp,
            ShrSIOp,
            CmpIOp,
            ExtSIOp,
            ExtUIOp,
            TruncIOp,
            BranchOp,
            CondBranchOp,
        ],
        types: [IntegerType, IndexType, UnitType],
    }
}

pub struct IntegerType {
    width: u32,
}

impl IntegerType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, width: u32) -> TypeId {
        context.get_type_id(Arc::new(Self { width }))
    }
    pub fn width(&self) -> u32 {
        self.width
    }
}

impl TypeConstraint for IntegerType {}

impl Type for IntegerType {
    fn dialect(&self) -> &'static str {
        "builtin"
    }

    fn parse_key() -> &'static str {
        "i"
    }

    fn parse<'src>(
        mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        use tir::parse::common::Cursor;
        if let Some(rest) = mnemonic.strip_prefix('i')
            && let Ok(width) = rest.parse::<u32>()
        {
            Ok(Self::new(context, width))
        } else {
            Err((parser.span(), Error::ExpectedType))
        }
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write(format!("i{}", self.width))
    }

    fn eq(&self, other: &dyn Type) -> bool {
        let int = (other as &dyn Any).downcast_ref::<IntegerType>();
        int.map(|item| item.width == self.width).unwrap_or(false)
    }
}

pub struct IndexType;

impl IndexType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for IndexType {}

impl Type for IndexType {
    fn dialect(&self) -> &'static str {
        "builtin"
    }

    fn parse_key() -> &'static str {
        "index"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("index")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<IndexType>().is_some()
    }
}

pub struct UnitType;

impl UnitType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for UnitType {}

impl Type for UnitType {
    fn dialect(&self) -> &'static str {
        "builtin"
    }

    fn parse_key() -> &'static str {
        "unit"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("unit")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<UnitType>().is_some()
    }
}

pub struct Integer<const N: usize>;

impl<const N: usize> TypeConstraint for Integer<N> {
    fn satisfies(ty: &dyn Type) -> bool
    where
        Self: Sized + 'static,
    {
        let int = (ty as &dyn Any).downcast_ref::<IntegerType>();
        if let Some(int) = int {
            int.width == N as u32
        } else {
            false
        }
    }
}
