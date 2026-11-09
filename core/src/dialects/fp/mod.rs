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

pub use arithmetic::*;
pub use bits::*;
pub use compare::*;
pub use constant::*;
pub use contract::{
    AccuracyRequirement, ErrorBound, EvaluationContract, IntermediateExceptions, ScopedFacts,
    TransformPermissions,
};
pub use convert::*;
pub use env::FloatEnvironment;
pub use env::*;
pub use semantics::{
    ArithmeticSemantics, ComparisonBehavior, ComparisonSemantics, Exceptions,
    IntegerConversionSemantics, InvalidConversion, NaNPolicy, Rounding, Semantics, SubnormalMode,
    Tininess,
};
mod contract;
pub use round::*;
pub use tir_adt::RoundingMode;
pub use types::{EnvironmentType, RoundingType};

pub mod ops {
    pub use super::arithmetic::*;
    pub use super::bits::*;
    pub use super::compare::*;
    pub use super::constant::*;
    pub use super::convert::*;
    pub use super::env::*;
    pub use super::round::*;
}

crate::dialect! {
    FpDialect {
        name: "fp",
        operations: [
            CONSTANT, ROUNDING_CONSTANT, GET_ROUND, SET_ROUND, GET_FLAGS, CLEAR_FLAGS,
            RAISE_FLAGS, GET_TRAPS, SET_TRAPS, SAVE, RESTORE, HOLD, UPDATE,
            ADD, SUB, MUL, DIV, FMA, SQRT, CONVERT, FROM_SI, FROM_UI, TO_SI,
            TO_UI, CMP, NEG, ABS, COPYSIGN, SIGNBIT, CLASSIFY, ROUND, FENCE
        ],
        types: [EnvironmentType, RoundingType],
    }
}
