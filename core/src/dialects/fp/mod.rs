//! Floating-point expressions whose computation type carries the evaluation policy.

use tir_symbolic::term::no_carrier;

use crate::builtin::FloatType;
use crate::ty::{Ctor, Kind, Param, Terms, TypeArg, TypeCtor, TypeSpelling};
use crate::{BlockHandle, Context, TypeId, ValueId, dialect};

#[allow(dead_code, unused_imports)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/fp.rs"));
}

pub use generated::{
    AddOp, AddOpBuilder, CaptureOp, CaptureOpBuilder, CmpOp, CmpOpBuilder, Contraction, ConvertOp,
    ConvertOpBuilder, DivOp, DivOpBuilder, FromSIOp, FromSIOpBuilder, FromUIOp, FromUIOpBuilder,
    MaterializeOp, MaterializeOpBuilder, MulOp, MulOpBuilder, Rounding, SubOp, SubOpBuilder,
    ToSIOp, ToSIOpBuilder, ToUIOp, ToUIOpBuilder, add, capture, cmp, convert, div, from_si,
    from_ui, materialize, mul, sub, to_si, to_ui,
};

dialect! {
    FpDialect {
        name: "fp",
        operations: [
            CaptureOp, MaterializeOp, AddOp, SubOp, MulOp, DivOp,
            CmpOp, ConvertOp, FromSIOp, FromUIOp, ToSIOp, ToUIOp,
        ],
        types: [ComputationType],
    }
}

static COMPUTATION: Ctor = Ctor::new(TypeCtor {
    dialect: "fp",
    name: "computation",
    params: &[
        ("storage", Param::One(Kind::Data)),
        ("rounding", Param::One(Kind::Sym)),
        ("contraction", Param::One(Kind::Sym)),
    ],
    kind: Kind::Data,
    validate: validate_computation,
    carrier: no_carrier,
});

/// An FP expression indexed by its storage format, rounding and contraction.
pub struct ComputationType;

impl ComputationType {
    /// Intern a computation type for an IEEE binary16, binary32 or binary64 storage type.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        context: &Context,
        storage: TypeId,
        rounding: Rounding,
        contraction: Contraction,
    ) -> TypeId {
        let terms = context.terms();
        terms
            .intern(
                COMPUTATION.id(terms),
                vec![
                    TypeArg::Type(storage),
                    TypeArg::Sym(terms.name(rounding.name())),
                    TypeArg::Sym(terms.name(contraction.name())),
                ],
            )
            .expect("a binary16, binary32 or binary64 computation type")
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        Ctor::spelling(context, &COMPUTATION)
    }
}

impl Rounding {
    fn name(self) -> &'static str {
        match self {
            Self::NearestEven => "nearest_even",
            Self::NearestAway => "nearest_away",
            Self::TowardZero => "toward_zero",
            Self::TowardPositive => "toward_positive",
            Self::TowardNegative => "toward_negative",
        }
    }
}

impl Contraction {
    fn name(self) -> &'static str {
        match self {
            Self::Separate => "separate",
            Self::Permitted => "permitted",
        }
    }
}

fn validate_computation(terms: &Terms, args: &[TypeArg]) -> Result<(), String> {
    let [
        storage @ (TypeArg::Type(_) | TypeArg::Var(_)),
        rounding @ (TypeArg::Sym(_) | TypeArg::Var(_)),
        contraction @ (TypeArg::Sym(_) | TypeArg::Var(_)),
    ] = args
    else {
        return Err(
            "fp.computation requires a storage type, rounding mode and contraction policy".into(),
        );
    };
    if let TypeArg::Type(storage) = storage
        && !is_storage(terms, *storage)
    {
        return Err("fp.computation storage type must be binary16, binary32 or binary64".into());
    }
    if let TypeArg::Sym(rounding) = rounding {
        let rounding = terms.name_text(*rounding);
        if ![
            Rounding::NearestEven,
            Rounding::NearestAway,
            Rounding::TowardZero,
            Rounding::TowardPositive,
            Rounding::TowardNegative,
        ]
        .into_iter()
        .any(|mode| mode.name() == rounding)
        {
            return Err(format!("unknown FP rounding mode '{rounding}'"));
        }
    }
    if let TypeArg::Sym(contraction) = contraction {
        let contraction = terms.name_text(*contraction);
        if ![Contraction::Separate, Contraction::Permitted]
            .into_iter()
            .any(|mode| mode.name() == contraction)
        {
            return Err(format!("unknown FP contraction policy '{contraction}'"));
        }
    }
    Ok(())
}

fn is_storage(terms: &Terms, ty: TypeId) -> bool {
    FloatType::view(terms, ty).is_some_and(|ty| {
        matches!(
            (ty.exp_width(), ty.mant_width()),
            (5, 10) | (8, 23) | (11, 52)
        )
    })
}

/// One arithmetic instruction evaluated at its source rounding boundary.
#[derive(Clone, Copy)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinaryOp {
    /// Append a single-instruction expression using nearest-even rounding and separate operations.
    pub fn materialize(
        self,
        context: &Context,
        block: &BlockHandle,
        lhs: ValueId,
        rhs: ValueId,
    ) -> ValueId {
        let storage = context.get_value(lhs).ty();
        let computation = ComputationType::new(
            context,
            storage,
            Rounding::NearestEven,
            Contraction::Separate,
        );
        let lhs = capture(context, lhs, computation)
            .at(block.end())
            .build()
            .result();
        let rhs = capture(context, rhs, computation)
            .at(block.end())
            .build()
            .result();
        let result = match self {
            Self::Add => add(context, lhs, rhs, computation)
                .at(block.end())
                .build()
                .result(),
            Self::Sub => sub(context, lhs, rhs, computation)
                .at(block.end())
                .build()
                .result(),
            Self::Mul => mul(context, lhs, rhs, computation)
                .at(block.end())
                .build()
                .result(),
            Self::Div => div(context, lhs, rhs, computation)
                .at(block.end())
                .build()
                .result(),
        };
        materialize(context, result, storage)
            .at(block.end())
            .build()
            .result()
    }
}
