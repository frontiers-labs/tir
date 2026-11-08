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
    pub use super::arithmetic::{AddOp, DivOp, FmaOp, MulOp, SqrtOp, SubOp};
    pub use super::bits::{AbsOp, ClassifyOp, CopySignOp, NegOp, SignBitOp};
    pub use super::compare::CmpOp;
    pub use super::constant::ConstantOp;
    pub use super::convert::{ConvertOp, FromSiOp, FromUiOp, ToSiOp, ToUiOp};
    pub use super::env::{
        ClearFlagsOp, GetFlagsOp, GetRoundOp, GetTrapsOp, HoldOp, RaiseFlagsOp, RestoreOp,
        RoundingConstantOp, SaveOp, SetRoundOp, SetTrapsOp, UpdateOp,
    };
    pub use super::round::{FenceOp, RoundOp};
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
