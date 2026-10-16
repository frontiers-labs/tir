#![allow(clippy::too_many_arguments)]

use tir::helpers::operation;
use tir::{OpId, TypeId, ValueId};

use tir;

operation! {
    ImageTexelPointerOp {
        name: "ImageTexelPointer",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { image, coordinate, sample },
        results: R { result }
    }
}

operation! {
    GenericPtrMemSemanticsOp {
        name: "GenericPtrMemSemantics",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { pointer },
        results: R { result }
    }
}

operation! {
    VectorExtractDynamicOp {
        name: "VectorExtractDynamic",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { vector, index },
        results: R { result }
    }
}

operation! {
    VectorInsertDynamicOp {
        name: "VectorInsertDynamic",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { vector, component, index },
        results: R { result }
    }
}

operation! {
    CopyObjectOp {
        name: "CopyObject",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { operand },
        results: R { result }
    }
}

operation! {
    TransposeOp {
        name: "Transpose",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { matrix },
        results: R { result }
    }
}

operation! {
    ConvertFToUOp {
        name: "ConvertFToU",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { float_value },
        results: R { result }
    }
}

operation! {
    ConvertFToSOp {
        name: "ConvertFToS",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { float_value },
        results: R { result }
    }
}

operation! {
    ConvertSToFOp {
        name: "ConvertSToF",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { signed_value },
        results: R { result }
    }
}

operation! {
    ConvertUToFOp {
        name: "ConvertUToF",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { unsigned_value },
        results: R { result }
    }
}

operation! {
    UConvertOp {
        name: "UConvert",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { unsigned_value },
        results: R { result }
    }
}

operation! {
    SConvertOp {
        name: "SConvert",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { signed_value },
        results: R { result }
    }
}

operation! {
    FConvertOp {
        name: "FConvert",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { float_value },
        results: R { result }
    }
}

operation! {
    QuantizeToF16Op {
        name: "QuantizeToF16",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { value },
        results: R { result }
    }
}

operation! {
    ConvertPtrToUOp {
        name: "ConvertPtrToU",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { pointer },
        results: R { result }
    }
}

operation! {
    SatConvertSToUOp {
        name: "SatConvertSToU",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { signed_value },
        results: R { result }
    }
}

operation! {
    SatConvertUToSOp {
        name: "SatConvertUToS",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { unsigned_value },
        results: R { result }
    }
}

operation! {
    ConvertUToPtrOp {
        name: "ConvertUToPtr",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { integer_value },
        results: R { result }
    }
}

operation! {
    PtrCastToGenericOp {
        name: "PtrCastToGeneric",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { pointer },
        results: R { result }
    }
}

operation! {
    GenericCastToPtrOp {
        name: "GenericCastToPtr",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { pointer },
        results: R { result }
    }
}

operation! {
    BitcastOp {
        name: "Bitcast",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { operand },
        results: R { result }
    }
}

operation! {
    SNegateOp {
        name: "SNegate",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { operand },
        results: R { result }
    }
}

operation! {
    FNegateOp {
        name: "FNegate",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { operand },
        results: R { result }
    }
}

operation! {
    IAddOp {
        name: "IAdd",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FAddOp {
        name: "FAdd",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    ISubOp {
        name: "ISub",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FSubOp {
        name: "FSub",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    IMulOp {
        name: "IMul",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FMulOp {
        name: "FMul",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    UDivOp {
        name: "UDiv",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SDivOp {
        name: "SDiv",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FDivOp {
        name: "FDiv",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    UModOp {
        name: "UMod",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SRemOp {
        name: "SRem",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SModOp {
        name: "SMod",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FRemOp {
        name: "FRem",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FModOp {
        name: "FMod",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    VectorTimesScalarOp {
        name: "VectorTimesScalar",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { vector, scalar },
        results: R { result }
    }
}

operation! {
    MatrixTimesScalarOp {
        name: "MatrixTimesScalar",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { matrix, scalar },
        results: R { result }
    }
}

operation! {
    VectorTimesMatrixOp {
        name: "VectorTimesMatrix",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { vector, matrix },
        results: R { result }
    }
}

operation! {
    MatrixTimesVectorOp {
        name: "MatrixTimesVector",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { matrix, vector },
        results: R { result }
    }
}

operation! {
    MatrixTimesMatrixOp {
        name: "MatrixTimesMatrix",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { leftmatrix, rightmatrix },
        results: R { result }
    }
}

operation! {
    OuterProductOp {
        name: "OuterProduct",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { vector_1, vector_2 },
        results: R { result }
    }
}

operation! {
    DotOp {
        name: "Dot",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { vector_1, vector_2 },
        results: R { result }
    }
}

operation! {
    IAddCarryOp {
        name: "IAddCarry",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    ISubBorrowOp {
        name: "ISubBorrow",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    UMulExtendedOp {
        name: "UMulExtended",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SMulExtendedOp {
        name: "SMulExtended",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    AnyOp {
        name: "Any",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { vector },
        results: R { result }
    }
}

operation! {
    AllOp {
        name: "All",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { vector },
        results: R { result }
    }
}

operation! {
    IsNanOp {
        name: "IsNan",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { x },
        results: R { result }
    }
}

operation! {
    IsInfOp {
        name: "IsInf",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { x },
        results: R { result }
    }
}

operation! {
    IsFiniteOp {
        name: "IsFinite",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { x },
        results: R { result }
    }
}

operation! {
    IsNormalOp {
        name: "IsNormal",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { x },
        results: R { result }
    }
}

operation! {
    SignBitSetOp {
        name: "SignBitSet",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { x },
        results: R { result }
    }
}

operation! {
    LessOrGreaterOp {
        name: "LessOrGreater",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { x, y },
        results: R { result }
    }
}

operation! {
    OrderedOp {
        name: "Ordered",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { x, y },
        results: R { result }
    }
}

operation! {
    UnorderedOp {
        name: "Unordered",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { x, y },
        results: R { result }
    }
}

operation! {
    LogicalEqualOp {
        name: "LogicalEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    LogicalNotEqualOp {
        name: "LogicalNotEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    LogicalOrOp {
        name: "LogicalOr",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    LogicalAndOp {
        name: "LogicalAnd",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    LogicalNotOp {
        name: "LogicalNot",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { operand },
        results: R { result }
    }
}

operation! {
    SelectOp {
        name: "Select",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { condition, object_1, object_2 },
        results: R { result }
    }
}

operation! {
    IEqualOp {
        name: "IEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    INotEqualOp {
        name: "INotEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    UGreaterThanOp {
        name: "UGreaterThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SGreaterThanOp {
        name: "SGreaterThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    UGreaterThanEqualOp {
        name: "UGreaterThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SGreaterThanEqualOp {
        name: "SGreaterThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    ULessThanOp {
        name: "ULessThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SLessThanOp {
        name: "SLessThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    ULessThanEqualOp {
        name: "ULessThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    SLessThanEqualOp {
        name: "SLessThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FOrdEqualOp {
        name: "FOrdEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FUnordEqualOp {
        name: "FUnordEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FOrdNotEqualOp {
        name: "FOrdNotEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FUnordNotEqualOp {
        name: "FUnordNotEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FOrdLessThanOp {
        name: "FOrdLessThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FUnordLessThanOp {
        name: "FUnordLessThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FOrdGreaterThanOp {
        name: "FOrdGreaterThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FUnordGreaterThanOp {
        name: "FUnordGreaterThan",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FOrdLessThanEqualOp {
        name: "FOrdLessThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FUnordLessThanEqualOp {
        name: "FUnordLessThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FOrdGreaterThanEqualOp {
        name: "FOrdGreaterThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    FUnordGreaterThanEqualOp {
        name: "FUnordGreaterThanEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    ShiftRightLogicalOp {
        name: "ShiftRightLogical",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { base, shift },
        results: R { result }
    }
}

operation! {
    ShiftRightArithmeticOp {
        name: "ShiftRightArithmetic",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { base, shift },
        results: R { result }
    }
}

operation! {
    ShiftLeftLogicalOp {
        name: "ShiftLeftLogical",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { base, shift },
        results: R { result }
    }
}

operation! {
    BitwiseOrOp {
        name: "BitwiseOr",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    BitwiseXorOp {
        name: "BitwiseXor",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    BitwiseAndOp {
        name: "BitwiseAnd",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    NotOp {
        name: "Not",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { operand },
        results: R { result }
    }
}

operation! {
    BitFieldInsertOp {
        name: "BitFieldInsert",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { base, insert, offset, count },
        results: R { result }
    }
}

operation! {
    BitFieldSExtractOp {
        name: "BitFieldSExtract",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { base, offset, count },
        results: R { result }
    }
}

operation! {
    BitFieldUExtractOp {
        name: "BitFieldUExtract",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { base, offset, count },
        results: R { result }
    }
}

operation! {
    BitReverseOp {
        name: "BitReverse",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { base },
        results: R { result }
    }
}

operation! {
    BitCountOp {
        name: "BitCount",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { base },
        results: R { result }
    }
}

operation! {
    AtomicLoadOp {
        name: "AtomicLoad",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { pointer, memory, semantics },
        results: R { result }
    }
}

operation! {
    AtomicExchangeOp {
        name: "AtomicExchange",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicCompareExchangeOp {
        name: "AtomicCompareExchange",
        dialect: "spirv",
        scheme: "(_, _, _, _, _, _) -> _",
        operands: O { pointer, memory, equal, unequal, value, comparator },
        results: R { result }
    }
}

operation! {
    AtomicCompareExchangeWeakOp {
        name: "AtomicCompareExchangeWeak",
        dialect: "spirv",
        scheme: "(_, _, _, _, _, _) -> _",
        operands: O { pointer, memory, equal, unequal, value, comparator },
        results: R { result }
    }
}

operation! {
    AtomicIIncrementOp {
        name: "AtomicIIncrement",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { pointer, memory, semantics },
        results: R { result }
    }
}

operation! {
    AtomicIDecrementOp {
        name: "AtomicIDecrement",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { pointer, memory, semantics },
        results: R { result }
    }
}

operation! {
    AtomicIAddOp {
        name: "AtomicIAdd",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicISubOp {
        name: "AtomicISub",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicSMinOp {
        name: "AtomicSMin",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicUMinOp {
        name: "AtomicUMin",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicSMaxOp {
        name: "AtomicSMax",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicUMaxOp {
        name: "AtomicUMax",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicAndOp {
        name: "AtomicAnd",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicOrOp {
        name: "AtomicOr",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicXorOp {
        name: "AtomicXor",
        dialect: "spirv",
        scheme: "(_, _, _, _) -> _",
        operands: O { pointer, memory, semantics, value },
        results: R { result }
    }
}

operation! {
    AtomicFlagTestAndSetOp {
        name: "AtomicFlagTestAndSet",
        dialect: "spirv",
        scheme: "(_, _, _) -> _",
        operands: O { pointer, memory, semantics },
        results: R { result }
    }
}

operation! {
    CopyLogicalOp {
        name: "CopyLogical",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { operand },
        results: R { result }
    }
}

operation! {
    PtrEqualOp {
        name: "PtrEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    PtrNotEqualOp {
        name: "PtrNotEqual",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

operation! {
    PtrDiffOp {
        name: "PtrDiff",
        dialect: "spirv",
        scheme: "(_, _) -> _",
        operands: O { operand_1, operand_2 },
        results: R { result }
    }
}

/// Every generated op: its SPIR-V opcode, its name in the `spirv` dialect, and
/// how many id operands follow the result type and result id.
static GENERATED: &[(u16, &str, usize)] = &[
    (60, "ImageTexelPointer", 3),
    (69, "GenericPtrMemSemantics", 1),
    (77, "VectorExtractDynamic", 2),
    (78, "VectorInsertDynamic", 3),
    (83, "CopyObject", 1),
    (84, "Transpose", 1),
    (109, "ConvertFToU", 1),
    (110, "ConvertFToS", 1),
    (111, "ConvertSToF", 1),
    (112, "ConvertUToF", 1),
    (113, "UConvert", 1),
    (114, "SConvert", 1),
    (115, "FConvert", 1),
    (116, "QuantizeToF16", 1),
    (117, "ConvertPtrToU", 1),
    (118, "SatConvertSToU", 1),
    (119, "SatConvertUToS", 1),
    (120, "ConvertUToPtr", 1),
    (121, "PtrCastToGeneric", 1),
    (122, "GenericCastToPtr", 1),
    (124, "Bitcast", 1),
    (126, "SNegate", 1),
    (127, "FNegate", 1),
    (128, "IAdd", 2),
    (129, "FAdd", 2),
    (130, "ISub", 2),
    (131, "FSub", 2),
    (132, "IMul", 2),
    (133, "FMul", 2),
    (134, "UDiv", 2),
    (135, "SDiv", 2),
    (136, "FDiv", 2),
    (137, "UMod", 2),
    (138, "SRem", 2),
    (139, "SMod", 2),
    (140, "FRem", 2),
    (141, "FMod", 2),
    (142, "VectorTimesScalar", 2),
    (143, "MatrixTimesScalar", 2),
    (144, "VectorTimesMatrix", 2),
    (145, "MatrixTimesVector", 2),
    (146, "MatrixTimesMatrix", 2),
    (147, "OuterProduct", 2),
    (148, "Dot", 2),
    (149, "IAddCarry", 2),
    (150, "ISubBorrow", 2),
    (151, "UMulExtended", 2),
    (152, "SMulExtended", 2),
    (154, "Any", 1),
    (155, "All", 1),
    (156, "IsNan", 1),
    (157, "IsInf", 1),
    (158, "IsFinite", 1),
    (159, "IsNormal", 1),
    (160, "SignBitSet", 1),
    (161, "LessOrGreater", 2),
    (162, "Ordered", 2),
    (163, "Unordered", 2),
    (164, "LogicalEqual", 2),
    (165, "LogicalNotEqual", 2),
    (166, "LogicalOr", 2),
    (167, "LogicalAnd", 2),
    (168, "LogicalNot", 1),
    (169, "Select", 3),
    (170, "IEqual", 2),
    (171, "INotEqual", 2),
    (172, "UGreaterThan", 2),
    (173, "SGreaterThan", 2),
    (174, "UGreaterThanEqual", 2),
    (175, "SGreaterThanEqual", 2),
    (176, "ULessThan", 2),
    (177, "SLessThan", 2),
    (178, "ULessThanEqual", 2),
    (179, "SLessThanEqual", 2),
    (180, "FOrdEqual", 2),
    (181, "FUnordEqual", 2),
    (182, "FOrdNotEqual", 2),
    (183, "FUnordNotEqual", 2),
    (184, "FOrdLessThan", 2),
    (185, "FUnordLessThan", 2),
    (186, "FOrdGreaterThan", 2),
    (187, "FUnordGreaterThan", 2),
    (188, "FOrdLessThanEqual", 2),
    (189, "FUnordLessThanEqual", 2),
    (190, "FOrdGreaterThanEqual", 2),
    (191, "FUnordGreaterThanEqual", 2),
    (194, "ShiftRightLogical", 2),
    (195, "ShiftRightArithmetic", 2),
    (196, "ShiftLeftLogical", 2),
    (197, "BitwiseOr", 2),
    (198, "BitwiseXor", 2),
    (199, "BitwiseAnd", 2),
    (200, "Not", 1),
    (201, "BitFieldInsert", 4),
    (202, "BitFieldSExtract", 3),
    (203, "BitFieldUExtract", 3),
    (204, "BitReverse", 1),
    (205, "BitCount", 1),
    (227, "AtomicLoad", 3),
    (229, "AtomicExchange", 4),
    (230, "AtomicCompareExchange", 6),
    (231, "AtomicCompareExchangeWeak", 6),
    (232, "AtomicIIncrement", 3),
    (233, "AtomicIDecrement", 3),
    (234, "AtomicIAdd", 4),
    (235, "AtomicISub", 4),
    (236, "AtomicSMin", 4),
    (237, "AtomicUMin", 4),
    (238, "AtomicSMax", 4),
    (239, "AtomicUMax", 4),
    (240, "AtomicAnd", 4),
    (241, "AtomicOr", 4),
    (242, "AtomicXor", 4),
    (318, "AtomicFlagTestAndSet", 3),
    (400, "CopyLogical", 1),
    (401, "PtrEqual", 2),
    (402, "PtrNotEqual", 2),
    (403, "PtrDiff", 2),
];

pub(crate) fn opcode_for_name(name: &str) -> Option<u16> {
    GENERATED
        .iter()
        .find(|(_, candidate, _)| *candidate == name)
        .map(|&(opcode, _, _)| opcode)
}

pub(crate) fn build_generated(
    context: &tir::Context,
    opcode: u16,
    operands: &[ValueId],
    result_type: TypeId,
) -> Option<(OpId, ValueId)> {
    let &(_, name, arity) = GENERATED.iter().find(|&&(code, _, _)| code == opcode)?;
    if operands.len() != arity {
        return None;
    }
    let result = context.create_value(result_type, None).id();
    let instance = tir::NewOp::new_dynamic(
        ("spirv", name),
        context.clone(),
        operands.to_vec(),
        vec![result],
        vec![],
        vec![],
    );
    let op = context.add_operation(instance);
    Some((op.id, result))
}
