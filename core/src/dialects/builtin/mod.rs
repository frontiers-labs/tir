mod arith;
mod float;
mod module;
mod state;
mod tuple;

use std::any::Any;
use std::sync::Arc;

use crate::ty::TypeConstraint;
use crate::{Context, Error, IRFormatter, TirType, Type, TypeId, dialect, parse::Span};

use crate as tir;

pub use arith::*;
pub use float::*;
pub use module::*;
pub use state::*;
pub use tuple::*;

pub mod ops {
    pub use super::arith::*;
    pub use super::float::*;
    pub use super::module::*;
    pub use super::state::*;
    pub use super::tuple::*;
}

dialect! {
    BuiltinDialect {
        name: "builtin",
        operations: [
            ModuleOp,
            ModuleEndOp,
            ConstantOp,
            AddIOp,
            SubIOp,
            MulIOp,
            DivSIOp,
            DivUIOp,
            RemSIOp,
            RemUIOp,
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
            BitcastOp,
            ConstantFOp,
            AddFOp,
            SubFOp,
            MulFOp,
            DivFOp,
            SIToFPOp,
            UIToFPOp,
            FPToSIOp,
            FPToUIOp,
            CmpFOp,
            MakeTupleOp,
            TupleGetOp,
        ],
        types: [IntegerType, FloatType, IndexType, UnitType, TokenType, TupleType, StateType],
    }
}

#[derive(TirType)]
#[tir_type(dialect = "builtin", name = "i")]
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

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        state.write_u32(self.width);
    }
}

/// A binary floating-point type described by its exponent and mantissa widths
/// (the mantissa excludes the implicit leading bit), matching
/// [`tir_adt::APFloat`]. The named formats are the IEEE 754 binary8-binary64
/// interchange formats (with both OCP FP8 encodings) plus bfloat16; any of them
/// is a valid operand of the shared `*f` arithmetic ops.
#[derive(TirType)]
#[tir_type(dialect = "builtin", name = "f")]
pub struct FloatType {
    exp_width: u32,
    mant_width: u32,
}

/// `(exp_width, mant_width, mnemonic)` for each named format.
const FLOAT_FORMATS: [(u32, u32, &str); 6] = [
    (4, 3, "f8e4m3"),
    (5, 2, "f8e5m2"),
    (5, 10, "f16"),
    (8, 7, "bf16"),
    (8, 23, "f32"),
    (11, 52, "f64"),
];

impl FloatType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, exp_width: u32, mant_width: u32) -> TypeId {
        context.get_type_id(Arc::new(Self {
            exp_width,
            mant_width,
        }))
    }

    pub fn f8e4m3(context: &Context) -> TypeId {
        Self::new(context, 4, 3)
    }

    pub fn f8e5m2(context: &Context) -> TypeId {
        Self::new(context, 5, 2)
    }

    pub fn f16(context: &Context) -> TypeId {
        Self::new(context, 5, 10)
    }

    pub fn bf16(context: &Context) -> TypeId {
        Self::new(context, 8, 7)
    }

    pub fn f32(context: &Context) -> TypeId {
        Self::new(context, 8, 23)
    }

    pub fn f64(context: &Context) -> TypeId {
        Self::new(context, 11, 52)
    }

    pub fn exp_width(&self) -> u32 {
        self.exp_width
    }

    pub fn mant_width(&self) -> u32 {
        self.mant_width
    }

    /// Total storage width, including the sign bit.
    pub fn bit_width(&self) -> u32 {
        1 + self.exp_width + self.mant_width
    }
}

impl TypeConstraint for FloatType {}

impl Type for FloatType {
    fn dialect(&self) -> &'static str {
        "builtin"
    }

    fn parse_key() -> &'static str {
        "f"
    }

    fn extra_parse_keys() -> &'static [&'static str] {
        &["bf"]
    }

    fn parse<'src>(
        mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        use tir::parse::common::Cursor;
        FLOAT_FORMATS
            .iter()
            .find(|(_, _, name)| *name == mnemonic)
            .map(|&(exp, mant, _)| Self::new(context, exp, mant))
            .ok_or((parser.span(), Error::ExpectedType))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        let name = FLOAT_FORMATS
            .iter()
            .find(|&&(exp, mant, _)| exp == self.exp_width && mant == self.mant_width)
            .map(|(_, _, name)| *name)
            .expect("only named float formats are printable");
        fmt.write(name)
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any)
            .downcast_ref::<FloatType>()
            .map(|f| f.exp_width == self.exp_width && f.mant_width == self.mant_width)
            .unwrap_or(false)
    }

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        state.write_u32(self.exp_width);
        state.write_u32(self.mant_width);
    }
}

#[derive(TirType)]
#[tir_type(dialect = "builtin", name = "index")]
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

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

#[derive(TirType)]
#[tir_type(dialect = "builtin", name = "unit")]
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

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

/// Opaque, parameterless type with no runtime representation.
///
/// A `!token` value carries no data; it exists purely to encode a static
/// def-use relationship between operations. Given a use of a token, its
/// definition is the semantic producer of that token, which lets passes thread
/// ordering, dependency or async-completion edges through the IR without
/// inventing dummy data values.
#[derive(TirType)]
#[tir_type(dialect = "builtin", name = "token")]
pub struct TokenType;

impl TokenType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for TokenType {}

impl Type for TokenType {
    fn dialect(&self) -> &'static str {
        "builtin"
    }

    fn parse_key() -> &'static str {
        "token"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("token")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<TokenType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
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
