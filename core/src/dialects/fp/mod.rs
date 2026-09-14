mod arithmetic;
mod bits;
mod compare;
mod constant;
mod convert;
mod env;
mod resource;
mod round;
mod semantics;
mod types;

use arithmetic::{AddOp, DivOp, FmaOp, MulOp, SqrtOp, SubOp};
use bits::{AbsOp, ClassifyOp, CopySignOp, NegOp, SignBitOp};
use compare::CmpOp;
use constant::ConstantOp;
pub use contract::{
    AccuracyRequirement, ErrorBound, EvaluationContract, IntermediateExceptions, ScopedFacts,
    TransformPermissions,
};
use convert::{ConvertOp, FromSiOp, FromUiOp, ToSiOp, ToUiOp};
pub use env::FloatEnvironment;
use env::{
    ClearFlagsOp, GetFlagsOp, GetRoundOp, GetTrapsOp, HoldOp, RaiseFlagsOp, RestoreOp,
    RoundingConstantOp, SaveOp, SetRoundOp, SetTrapsOp, UpdateOp,
};
pub use semantics::{
    ArithmeticSemantics, ComparisonBehavior, ComparisonSemantics, Exceptions,
    IntegerConversionSemantics, InvalidConversion, NaNPolicy, Rounding, Semantics, SubnormalMode,
    Tininess,
};
mod contract;
use round::{FenceOp, RoundOp};
pub use tir_adt::RoundingMode;
pub use types::{EnvironmentType, RoundingType};

pub mod ops {
    pub use super::arithmetic::{
        AddOp, AddOpBuilder, DivOp, DivOpBuilder, FmaOp, FmaOpBuilder, MulOp, MulOpBuilder, SqrtOp,
        SqrtOpBuilder, SubOp, SubOpBuilder,
    };
    pub use super::bits::{
        AbsOp, AbsOpBuilder, ClassifyOp, ClassifyOpBuilder, CopySignOp, CopySignOpBuilder, NegOp,
        NegOpBuilder, SignBitOp, SignBitOpBuilder,
    };
    pub use super::compare::{CmpOp, CmpOpBuilder};
    pub use super::constant::{ConstantOp, ConstantOpBuilder};
    pub use super::convert::{
        ConvertOp, ConvertOpBuilder, FromSiOp, FromSiOpBuilder, FromUiOp, FromUiOpBuilder, ToSiOp,
        ToSiOpBuilder, ToUiOp, ToUiOpBuilder,
    };
    pub use super::env::{
        ClearFlagsOp, ClearFlagsOpBuilder, GetFlagsOp, GetFlagsOpBuilder, GetRoundOp,
        GetRoundOpBuilder, GetTrapsOp, GetTrapsOpBuilder, HoldOp, HoldOpBuilder, RaiseFlagsOp,
        RaiseFlagsOpBuilder, RestoreOp, RestoreOpBuilder, RoundingConstantOp,
        RoundingConstantOpBuilder, SaveOp, SaveOpBuilder, SetRoundOp, SetRoundOpBuilder,
        SetTrapsOp, SetTrapsOpBuilder, UpdateOp, UpdateOpBuilder,
    };
    pub use super::round::{FenceOp, FenceOpBuilder, RoundOp, RoundOpBuilder};
}

crate::dialect! {
    FpDialect {
        name: "fp",
        operations: [
            ConstantOp, RoundingConstantOp, GetRoundOp, SetRoundOp, GetFlagsOp, ClearFlagsOp,
            RaiseFlagsOp, GetTrapsOp, SetTrapsOp, SaveOp, RestoreOp, HoldOp, UpdateOp,
            AddOp, SubOp, MulOp, DivOp, FmaOp, SqrtOp, ConvertOp, FromSiOp, FromUiOp, ToSiOp,
            ToUiOp, CmpOp, NegOp, AbsOp, CopySignOp, SignBitOp, ClassifyOp, RoundOp, FenceOp
        ],
        types: [EnvironmentType, RoundingType],
    }
}
