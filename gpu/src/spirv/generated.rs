#![allow(clippy::too_many_arguments)]

use tir::helpers::operation;
use tir::{Any as TirAny, OpId, TypeId, ValueId};

use tir;

operation! {
    ImageTexelPointerOp {
        name: "ImageTexelPointer",
        dialect: "spirv",
        operands: O { image: "TirAny", coordinate: "TirAny", sample: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    GenericPtrMemSemanticsOp {
        name: "GenericPtrMemSemantics",
        dialect: "spirv",
        operands: O { pointer: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    VectorExtractDynamicOp {
        name: "VectorExtractDynamic",
        dialect: "spirv",
        operands: O { vector: "TirAny", index: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    VectorInsertDynamicOp {
        name: "VectorInsertDynamic",
        dialect: "spirv",
        operands: O { vector: "TirAny", component: "TirAny", index: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    CopyObjectOp {
        name: "CopyObject",
        dialect: "spirv",
        operands: O { operand: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    TransposeOp {
        name: "Transpose",
        dialect: "spirv",
        operands: O { matrix: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ConvertFToUOp {
        name: "ConvertFToU",
        dialect: "spirv",
        operands: O { float_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ConvertFToSOp {
        name: "ConvertFToS",
        dialect: "spirv",
        operands: O { float_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ConvertSToFOp {
        name: "ConvertSToF",
        dialect: "spirv",
        operands: O { signed_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ConvertUToFOp {
        name: "ConvertUToF",
        dialect: "spirv",
        operands: O { unsigned_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    UConvertOp {
        name: "UConvert",
        dialect: "spirv",
        operands: O { unsigned_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SConvertOp {
        name: "SConvert",
        dialect: "spirv",
        operands: O { signed_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FConvertOp {
        name: "FConvert",
        dialect: "spirv",
        operands: O { float_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    QuantizeToF16Op {
        name: "QuantizeToF16",
        dialect: "spirv",
        operands: O { value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ConvertPtrToUOp {
        name: "ConvertPtrToU",
        dialect: "spirv",
        operands: O { pointer: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SatConvertSToUOp {
        name: "SatConvertSToU",
        dialect: "spirv",
        operands: O { signed_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SatConvertUToSOp {
        name: "SatConvertUToS",
        dialect: "spirv",
        operands: O { unsigned_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ConvertUToPtrOp {
        name: "ConvertUToPtr",
        dialect: "spirv",
        operands: O { integer_value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    PtrCastToGenericOp {
        name: "PtrCastToGeneric",
        dialect: "spirv",
        operands: O { pointer: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    GenericCastToPtrOp {
        name: "GenericCastToPtr",
        dialect: "spirv",
        operands: O { pointer: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitcastOp {
        name: "Bitcast",
        dialect: "spirv",
        operands: O { operand: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SNegateOp {
        name: "SNegate",
        dialect: "spirv",
        operands: O { operand: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FNegateOp {
        name: "FNegate",
        dialect: "spirv",
        operands: O { operand: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IAddOp {
        name: "IAdd",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FAddOp {
        name: "FAdd",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ISubOp {
        name: "ISub",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FSubOp {
        name: "FSub",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IMulOp {
        name: "IMul",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FMulOp {
        name: "FMul",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    UDivOp {
        name: "UDiv",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SDivOp {
        name: "SDiv",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FDivOp {
        name: "FDiv",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    UModOp {
        name: "UMod",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SRemOp {
        name: "SRem",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SModOp {
        name: "SMod",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FRemOp {
        name: "FRem",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FModOp {
        name: "FMod",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    VectorTimesScalarOp {
        name: "VectorTimesScalar",
        dialect: "spirv",
        operands: O { vector: "TirAny", scalar: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    MatrixTimesScalarOp {
        name: "MatrixTimesScalar",
        dialect: "spirv",
        operands: O { matrix: "TirAny", scalar: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    VectorTimesMatrixOp {
        name: "VectorTimesMatrix",
        dialect: "spirv",
        operands: O { vector: "TirAny", matrix: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    MatrixTimesVectorOp {
        name: "MatrixTimesVector",
        dialect: "spirv",
        operands: O { matrix: "TirAny", vector: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    MatrixTimesMatrixOp {
        name: "MatrixTimesMatrix",
        dialect: "spirv",
        operands: O { leftmatrix: "TirAny", rightmatrix: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    OuterProductOp {
        name: "OuterProduct",
        dialect: "spirv",
        operands: O { vector_1: "TirAny", vector_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    DotOp {
        name: "Dot",
        dialect: "spirv",
        operands: O { vector_1: "TirAny", vector_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IAddCarryOp {
        name: "IAddCarry",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ISubBorrowOp {
        name: "ISubBorrow",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    UMulExtendedOp {
        name: "UMulExtended",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SMulExtendedOp {
        name: "SMulExtended",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AnyOp {
        name: "Any",
        dialect: "spirv",
        operands: O { vector: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AllOp {
        name: "All",
        dialect: "spirv",
        operands: O { vector: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IsNanOp {
        name: "IsNan",
        dialect: "spirv",
        operands: O { x: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IsInfOp {
        name: "IsInf",
        dialect: "spirv",
        operands: O { x: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IsFiniteOp {
        name: "IsFinite",
        dialect: "spirv",
        operands: O { x: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IsNormalOp {
        name: "IsNormal",
        dialect: "spirv",
        operands: O { x: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SignBitSetOp {
        name: "SignBitSet",
        dialect: "spirv",
        operands: O { x: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    LessOrGreaterOp {
        name: "LessOrGreater",
        dialect: "spirv",
        operands: O { x: "TirAny", y: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    OrderedOp {
        name: "Ordered",
        dialect: "spirv",
        operands: O { x: "TirAny", y: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    UnorderedOp {
        name: "Unordered",
        dialect: "spirv",
        operands: O { x: "TirAny", y: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    LogicalEqualOp {
        name: "LogicalEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    LogicalNotEqualOp {
        name: "LogicalNotEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    LogicalOrOp {
        name: "LogicalOr",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    LogicalAndOp {
        name: "LogicalAnd",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    LogicalNotOp {
        name: "LogicalNot",
        dialect: "spirv",
        operands: O { operand: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SelectOp {
        name: "Select",
        dialect: "spirv",
        operands: O { condition: "TirAny", object_1: "TirAny", object_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    IEqualOp {
        name: "IEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    INotEqualOp {
        name: "INotEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    UGreaterThanOp {
        name: "UGreaterThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SGreaterThanOp {
        name: "SGreaterThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    UGreaterThanEqualOp {
        name: "UGreaterThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SGreaterThanEqualOp {
        name: "SGreaterThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ULessThanOp {
        name: "ULessThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SLessThanOp {
        name: "SLessThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ULessThanEqualOp {
        name: "ULessThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    SLessThanEqualOp {
        name: "SLessThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FOrdEqualOp {
        name: "FOrdEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FUnordEqualOp {
        name: "FUnordEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FOrdNotEqualOp {
        name: "FOrdNotEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FUnordNotEqualOp {
        name: "FUnordNotEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FOrdLessThanOp {
        name: "FOrdLessThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FUnordLessThanOp {
        name: "FUnordLessThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FOrdGreaterThanOp {
        name: "FOrdGreaterThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FUnordGreaterThanOp {
        name: "FUnordGreaterThan",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FOrdLessThanEqualOp {
        name: "FOrdLessThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FUnordLessThanEqualOp {
        name: "FUnordLessThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FOrdGreaterThanEqualOp {
        name: "FOrdGreaterThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    FUnordGreaterThanEqualOp {
        name: "FUnordGreaterThanEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ShiftRightLogicalOp {
        name: "ShiftRightLogical",
        dialect: "spirv",
        operands: O { base: "TirAny", shift: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ShiftRightArithmeticOp {
        name: "ShiftRightArithmetic",
        dialect: "spirv",
        operands: O { base: "TirAny", shift: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    ShiftLeftLogicalOp {
        name: "ShiftLeftLogical",
        dialect: "spirv",
        operands: O { base: "TirAny", shift: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitwiseOrOp {
        name: "BitwiseOr",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitwiseXorOp {
        name: "BitwiseXor",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitwiseAndOp {
        name: "BitwiseAnd",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    NotOp {
        name: "Not",
        dialect: "spirv",
        operands: O { operand: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitFieldInsertOp {
        name: "BitFieldInsert",
        dialect: "spirv",
        operands: O { base: "TirAny", insert: "TirAny", offset: "TirAny", count: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitFieldSExtractOp {
        name: "BitFieldSExtract",
        dialect: "spirv",
        operands: O { base: "TirAny", offset: "TirAny", count: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitFieldUExtractOp {
        name: "BitFieldUExtract",
        dialect: "spirv",
        operands: O { base: "TirAny", offset: "TirAny", count: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitReverseOp {
        name: "BitReverse",
        dialect: "spirv",
        operands: O { base: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    BitCountOp {
        name: "BitCount",
        dialect: "spirv",
        operands: O { base: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicLoadOp {
        name: "AtomicLoad",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicExchangeOp {
        name: "AtomicExchange",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicCompareExchangeOp {
        name: "AtomicCompareExchange",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", equal: "TirAny", unequal: "TirAny", value: "TirAny", comparator: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicCompareExchangeWeakOp {
        name: "AtomicCompareExchangeWeak",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", equal: "TirAny", unequal: "TirAny", value: "TirAny", comparator: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicIIncrementOp {
        name: "AtomicIIncrement",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicIDecrementOp {
        name: "AtomicIDecrement",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicIAddOp {
        name: "AtomicIAdd",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicISubOp {
        name: "AtomicISub",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicSMinOp {
        name: "AtomicSMin",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicUMinOp {
        name: "AtomicUMin",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicSMaxOp {
        name: "AtomicSMax",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicUMaxOp {
        name: "AtomicUMax",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicAndOp {
        name: "AtomicAnd",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicOrOp {
        name: "AtomicOr",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicXorOp {
        name: "AtomicXor",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", value: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    AtomicFlagTestAndSetOp {
        name: "AtomicFlagTestAndSet",
        dialect: "spirv",
        operands: O { pointer: "TirAny", memory: "TirAny", semantics: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    CopyLogicalOp {
        name: "CopyLogical",
        dialect: "spirv",
        operands: O { operand: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    PtrEqualOp {
        name: "PtrEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    PtrNotEqualOp {
        name: "PtrNotEqual",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
    }
}

operation! {
    PtrDiffOp {
        name: "PtrDiff",
        dialect: "spirv",
        operands: O { operand_1: "TirAny", operand_2: "TirAny", },
        results: R { result: "TirAny", }
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
        context.as_context_ref(),
        operands.to_vec(),
        vec![result],
        vec![],
        vec![],
    );
    let op = context.add_operation(instance);
    Some((op.id, result))
}
