mod arith;
mod fn_type;
mod global;
mod module;
mod state;
mod tuple;
mod type_def;
mod ub;

use tir_symbolic::term::{ctor, no_carrier, valid};

use crate::ty::{Ctor, Kind, Terms, TypeArg, TypeCtor, TypeSpelling};
use crate::{Context, Error, TypeId, dialect};

use crate as tir;

pub use arith::*;
pub use fn_type::*;
pub use global::*;
pub use module::*;
pub use state::*;
pub use tuple::*;
pub use type_def::*;
pub use ub::*;

pub mod ops {
    pub use super::arith::*;
    pub use super::fn_type::*;
    pub use super::global::*;
    pub use super::module::*;
    pub use super::tuple::*;
    pub use super::type_def::{TypeDefOp, TypeDefOpBuilder, type_def};
    pub use super::ub::*;
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
            MakeTupleOp,
            TupleGetOp,
            GlobalOp,
            FnToPtrOp,
            PtrToFnOp,
            AssumeOp,
            FreezeOp,
            RefineOp,
        ],
        types: [
            IntegerType, FloatType, IndexType, UnitType, TupleType, FnType, VarArgsType, StateType,
            NamedType, AggregateType(AggregateKind::Struct), AggregateType(AggregateKind::Union),
            AggregateType(AggregateKind::Variant),
        ],
    }
}

/// `builtin.i<W>`, written `!iW`: a bit vector of `W` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegerType {
    width: u64,
}

impl IntegerType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, width: u64) -> TypeId {
        context.terms().int(width)
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let width = terms.as_ref().int_width(ty)?;
        Some(Self { width })
    }

    pub fn width(&self) -> u64 {
        self.width
    }

    pub fn spelling(_context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: ctor::INT,
            keys: &["i"],
            parse: |mnemonic, parser, context| {
                use tir::parse::common::Cursor;
                mnemonic
                    .strip_prefix('i')
                    .and_then(|rest| rest.parse::<u64>().ok())
                    .map(|width| Self::new(context, width))
                    .ok_or((parser.span(), Error::ExpectedType))
            },
            print: |spellings, ty, fmt| match Self::view(spellings, ty) {
                Some(int) => fmt.write(format!("i{}", int.width)),
                None => spellings.print_generic(ty, fmt),
            },
            variadic_tail: false,
        }
    }
}

/// A binary floating-point type described by its exponent and mantissa widths
/// (the mantissa excludes the implicit leading bit), matching
/// [`tir_adt::APFloat`]. The named formats are the IEEE 754 binary8-binary64
/// interchange formats (with both OCP FP8 encodings) plus bfloat16; any of them
/// is a valid operand of the shared `*f` arithmetic ops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        let args = vec![
            TypeArg::Nat(u64::from(exp_width)),
            TypeArg::Nat(u64::from(mant_width)),
        ];
        context
            .terms()
            .intern(ctor::FLOAT, args)
            .expect("a float type")
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

    /// The format a mnemonic (`f32`, `bf16`) names.
    pub fn named(context: &Context, mnemonic: &str) -> Option<TypeId> {
        let (exponent, mantissa, _) = FLOAT_FORMATS
            .iter()
            .find(|(_, _, name)| *name == mnemonic)?;
        Some(
            context
                .terms()
                .float(u64::from(*exponent), u64::from(*mantissa)),
        )
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        if terms.ctor(ty) != ctor::FLOAT {
            return None;
        }
        Some(Self {
            exp_width: u32::try_from(terms.nat(ty, 0)?).ok()?,
            mant_width: u32::try_from(terms.nat(ty, 1)?).ok()?,
        })
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

    pub fn spelling(_context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: ctor::FLOAT,
            keys: &["f", "bf"],
            parse: |mnemonic, parser, context| {
                use tir::parse::common::Cursor;
                // A format with no name of its own prints as `builtin.f<E, M>`.
                if mnemonic == "f" {
                    return crate::ty::parse_generic(ctor::FLOAT, parser, context);
                }
                FLOAT_FORMATS
                    .iter()
                    .find(|(_, _, name)| *name == mnemonic)
                    .map(|&(exp, mant, _)| Self::new(context, exp, mant))
                    .ok_or((parser.span(), Error::ExpectedType))
            },
            print: |spellings, ty, fmt| {
                let named = Self::view(spellings, ty).and_then(|float| {
                    FLOAT_FORMATS
                        .iter()
                        .find(|&&(exp, mant, _)| exp == float.exp_width && mant == float.mant_width)
                });
                match named {
                    Some((_, _, name)) => fmt.write(name),
                    None => spellings.print_generic(ty, fmt),
                }
            },
            variadic_tail: false,
        }
    }
}

static INDEX: Ctor = Ctor::new(TypeCtor {
    dialect: "builtin",
    name: "index",
    params: &[],
    kind: Kind::Data,
    validate: valid,
    carrier: no_carrier,
});

/// `builtin.index`: an address-sized count whose width the target picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexType;

impl IndexType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        let terms = context.terms();
        terms
            .intern(INDEX.id(terms), vec![])
            .expect("the index type")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        INDEX.is(terms, ty).then_some(Self)
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: INDEX.id(context),
            keys: &["index"],
            parse: |_, _, context| Ok(Self::new(context)),
            print: |_, _, fmt| fmt.write("index"),
            variadic_tail: false,
        }
    }
}

/// `builtin.unit`: the type with one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitType;

impl UnitType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context
            .terms()
            .intern(ctor::UNIT, vec![])
            .expect("the unit type")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        (terms.as_ref().ctor(ty) == ctor::UNIT).then_some(Self)
    }

    pub fn spelling(_context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: ctor::UNIT,
            keys: &["unit"],
            parse: |_, _, context| Ok(Self::new(context)),
            print: |_, _, fmt| fmt.write("unit"),
            variadic_tail: false,
        }
    }
}
