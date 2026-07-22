#![allow(clippy::too_many_arguments)]

use tir::helpers::operation;
use tir::{Any as TirAny, OpId, Operation, TypeId, ValueId};

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

pub(crate) fn opcode_for_name(name: &str) -> Option<u16> {
    Some(match name {
        "ImageTexelPointer" => 60,
        "GenericPtrMemSemantics" => 69,
        "VectorExtractDynamic" => 77,
        "VectorInsertDynamic" => 78,
        "CopyObject" => 83,
        "Transpose" => 84,
        "ConvertFToU" => 109,
        "ConvertFToS" => 110,
        "ConvertSToF" => 111,
        "ConvertUToF" => 112,
        "UConvert" => 113,
        "SConvert" => 114,
        "FConvert" => 115,
        "QuantizeToF16" => 116,
        "ConvertPtrToU" => 117,
        "SatConvertSToU" => 118,
        "SatConvertUToS" => 119,
        "ConvertUToPtr" => 120,
        "PtrCastToGeneric" => 121,
        "GenericCastToPtr" => 122,
        "Bitcast" => 124,
        "SNegate" => 126,
        "FNegate" => 127,
        "IAdd" => 128,
        "FAdd" => 129,
        "ISub" => 130,
        "FSub" => 131,
        "IMul" => 132,
        "FMul" => 133,
        "UDiv" => 134,
        "SDiv" => 135,
        "FDiv" => 136,
        "UMod" => 137,
        "SRem" => 138,
        "SMod" => 139,
        "FRem" => 140,
        "FMod" => 141,
        "VectorTimesScalar" => 142,
        "MatrixTimesScalar" => 143,
        "VectorTimesMatrix" => 144,
        "MatrixTimesVector" => 145,
        "MatrixTimesMatrix" => 146,
        "OuterProduct" => 147,
        "Dot" => 148,
        "IAddCarry" => 149,
        "ISubBorrow" => 150,
        "UMulExtended" => 151,
        "SMulExtended" => 152,
        "Any" => 154,
        "All" => 155,
        "IsNan" => 156,
        "IsInf" => 157,
        "IsFinite" => 158,
        "IsNormal" => 159,
        "SignBitSet" => 160,
        "LessOrGreater" => 161,
        "Ordered" => 162,
        "Unordered" => 163,
        "LogicalEqual" => 164,
        "LogicalNotEqual" => 165,
        "LogicalOr" => 166,
        "LogicalAnd" => 167,
        "LogicalNot" => 168,
        "Select" => 169,
        "IEqual" => 170,
        "INotEqual" => 171,
        "UGreaterThan" => 172,
        "SGreaterThan" => 173,
        "UGreaterThanEqual" => 174,
        "SGreaterThanEqual" => 175,
        "ULessThan" => 176,
        "SLessThan" => 177,
        "ULessThanEqual" => 178,
        "SLessThanEqual" => 179,
        "FOrdEqual" => 180,
        "FUnordEqual" => 181,
        "FOrdNotEqual" => 182,
        "FUnordNotEqual" => 183,
        "FOrdLessThan" => 184,
        "FUnordLessThan" => 185,
        "FOrdGreaterThan" => 186,
        "FUnordGreaterThan" => 187,
        "FOrdLessThanEqual" => 188,
        "FUnordLessThanEqual" => 189,
        "FOrdGreaterThanEqual" => 190,
        "FUnordGreaterThanEqual" => 191,
        "ShiftRightLogical" => 194,
        "ShiftRightArithmetic" => 195,
        "ShiftLeftLogical" => 196,
        "BitwiseOr" => 197,
        "BitwiseXor" => 198,
        "BitwiseAnd" => 199,
        "Not" => 200,
        "BitFieldInsert" => 201,
        "BitFieldSExtract" => 202,
        "BitFieldUExtract" => 203,
        "BitReverse" => 204,
        "BitCount" => 205,
        "AtomicLoad" => 227,
        "AtomicExchange" => 229,
        "AtomicCompareExchange" => 230,
        "AtomicCompareExchangeWeak" => 231,
        "AtomicIIncrement" => 232,
        "AtomicIDecrement" => 233,
        "AtomicIAdd" => 234,
        "AtomicISub" => 235,
        "AtomicSMin" => 236,
        "AtomicUMin" => 237,
        "AtomicSMax" => 238,
        "AtomicUMax" => 239,
        "AtomicAnd" => 240,
        "AtomicOr" => 241,
        "AtomicXor" => 242,
        "AtomicFlagTestAndSet" => 318,
        "CopyLogical" => 400,
        "PtrEqual" => 401,
        "PtrNotEqual" => 402,
        "PtrDiff" => 403,
        _ => return None,
    })
}

pub(crate) fn build_generated(
    context: &tir::Context,
    opcode: u16,
    operands: &[ValueId],
    result_type: TypeId,
) -> Option<(OpId, ValueId)> {
    Some(match opcode {
        60 if operands.len() == 3 => {
            let op = ImageTexelPointerOpBuilder::new(context)
                .image(operands[0])
                .coordinate(operands[1])
                .sample(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        69 if operands.len() == 1 => {
            let op = GenericPtrMemSemanticsOpBuilder::new(context)
                .pointer(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        77 if operands.len() == 2 => {
            let op = VectorExtractDynamicOpBuilder::new(context)
                .vector(operands[0])
                .index(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        78 if operands.len() == 3 => {
            let op = VectorInsertDynamicOpBuilder::new(context)
                .vector(operands[0])
                .component(operands[1])
                .index(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        83 if operands.len() == 1 => {
            let op = CopyObjectOpBuilder::new(context)
                .operand(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        84 if operands.len() == 1 => {
            let op = TransposeOpBuilder::new(context)
                .matrix(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        109 if operands.len() == 1 => {
            let op = ConvertFToUOpBuilder::new(context)
                .float_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        110 if operands.len() == 1 => {
            let op = ConvertFToSOpBuilder::new(context)
                .float_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        111 if operands.len() == 1 => {
            let op = ConvertSToFOpBuilder::new(context)
                .signed_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        112 if operands.len() == 1 => {
            let op = ConvertUToFOpBuilder::new(context)
                .unsigned_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        113 if operands.len() == 1 => {
            let op = UConvertOpBuilder::new(context)
                .unsigned_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        114 if operands.len() == 1 => {
            let op = SConvertOpBuilder::new(context)
                .signed_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        115 if operands.len() == 1 => {
            let op = FConvertOpBuilder::new(context)
                .float_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        116 if operands.len() == 1 => {
            let op = QuantizeToF16OpBuilder::new(context)
                .value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        117 if operands.len() == 1 => {
            let op = ConvertPtrToUOpBuilder::new(context)
                .pointer(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        118 if operands.len() == 1 => {
            let op = SatConvertSToUOpBuilder::new(context)
                .signed_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        119 if operands.len() == 1 => {
            let op = SatConvertUToSOpBuilder::new(context)
                .unsigned_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        120 if operands.len() == 1 => {
            let op = ConvertUToPtrOpBuilder::new(context)
                .integer_value(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        121 if operands.len() == 1 => {
            let op = PtrCastToGenericOpBuilder::new(context)
                .pointer(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        122 if operands.len() == 1 => {
            let op = GenericCastToPtrOpBuilder::new(context)
                .pointer(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        124 if operands.len() == 1 => {
            let op = BitcastOpBuilder::new(context)
                .operand(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        126 if operands.len() == 1 => {
            let op = SNegateOpBuilder::new(context)
                .operand(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        127 if operands.len() == 1 => {
            let op = FNegateOpBuilder::new(context)
                .operand(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        128 if operands.len() == 2 => {
            let op = IAddOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        129 if operands.len() == 2 => {
            let op = FAddOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        130 if operands.len() == 2 => {
            let op = ISubOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        131 if operands.len() == 2 => {
            let op = FSubOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        132 if operands.len() == 2 => {
            let op = IMulOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        133 if operands.len() == 2 => {
            let op = FMulOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        134 if operands.len() == 2 => {
            let op = UDivOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        135 if operands.len() == 2 => {
            let op = SDivOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        136 if operands.len() == 2 => {
            let op = FDivOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        137 if operands.len() == 2 => {
            let op = UModOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        138 if operands.len() == 2 => {
            let op = SRemOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        139 if operands.len() == 2 => {
            let op = SModOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        140 if operands.len() == 2 => {
            let op = FRemOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        141 if operands.len() == 2 => {
            let op = FModOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        142 if operands.len() == 2 => {
            let op = VectorTimesScalarOpBuilder::new(context)
                .vector(operands[0])
                .scalar(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        143 if operands.len() == 2 => {
            let op = MatrixTimesScalarOpBuilder::new(context)
                .matrix(operands[0])
                .scalar(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        144 if operands.len() == 2 => {
            let op = VectorTimesMatrixOpBuilder::new(context)
                .vector(operands[0])
                .matrix(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        145 if operands.len() == 2 => {
            let op = MatrixTimesVectorOpBuilder::new(context)
                .matrix(operands[0])
                .vector(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        146 if operands.len() == 2 => {
            let op = MatrixTimesMatrixOpBuilder::new(context)
                .leftmatrix(operands[0])
                .rightmatrix(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        147 if operands.len() == 2 => {
            let op = OuterProductOpBuilder::new(context)
                .vector_1(operands[0])
                .vector_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        148 if operands.len() == 2 => {
            let op = DotOpBuilder::new(context)
                .vector_1(operands[0])
                .vector_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        149 if operands.len() == 2 => {
            let op = IAddCarryOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        150 if operands.len() == 2 => {
            let op = ISubBorrowOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        151 if operands.len() == 2 => {
            let op = UMulExtendedOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        152 if operands.len() == 2 => {
            let op = SMulExtendedOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        154 if operands.len() == 1 => {
            let op = AnyOpBuilder::new(context)
                .vector(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        155 if operands.len() == 1 => {
            let op = AllOpBuilder::new(context)
                .vector(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        156 if operands.len() == 1 => {
            let op = IsNanOpBuilder::new(context)
                .x(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        157 if operands.len() == 1 => {
            let op = IsInfOpBuilder::new(context)
                .x(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        158 if operands.len() == 1 => {
            let op = IsFiniteOpBuilder::new(context)
                .x(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        159 if operands.len() == 1 => {
            let op = IsNormalOpBuilder::new(context)
                .x(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        160 if operands.len() == 1 => {
            let op = SignBitSetOpBuilder::new(context)
                .x(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        161 if operands.len() == 2 => {
            let op = LessOrGreaterOpBuilder::new(context)
                .x(operands[0])
                .y(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        162 if operands.len() == 2 => {
            let op = OrderedOpBuilder::new(context)
                .x(operands[0])
                .y(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        163 if operands.len() == 2 => {
            let op = UnorderedOpBuilder::new(context)
                .x(operands[0])
                .y(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        164 if operands.len() == 2 => {
            let op = LogicalEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        165 if operands.len() == 2 => {
            let op = LogicalNotEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        166 if operands.len() == 2 => {
            let op = LogicalOrOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        167 if operands.len() == 2 => {
            let op = LogicalAndOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        168 if operands.len() == 1 => {
            let op = LogicalNotOpBuilder::new(context)
                .operand(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        169 if operands.len() == 3 => {
            let op = SelectOpBuilder::new(context)
                .condition(operands[0])
                .object_1(operands[1])
                .object_2(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        170 if operands.len() == 2 => {
            let op = IEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        171 if operands.len() == 2 => {
            let op = INotEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        172 if operands.len() == 2 => {
            let op = UGreaterThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        173 if operands.len() == 2 => {
            let op = SGreaterThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        174 if operands.len() == 2 => {
            let op = UGreaterThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        175 if operands.len() == 2 => {
            let op = SGreaterThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        176 if operands.len() == 2 => {
            let op = ULessThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        177 if operands.len() == 2 => {
            let op = SLessThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        178 if operands.len() == 2 => {
            let op = ULessThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        179 if operands.len() == 2 => {
            let op = SLessThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        180 if operands.len() == 2 => {
            let op = FOrdEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        181 if operands.len() == 2 => {
            let op = FUnordEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        182 if operands.len() == 2 => {
            let op = FOrdNotEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        183 if operands.len() == 2 => {
            let op = FUnordNotEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        184 if operands.len() == 2 => {
            let op = FOrdLessThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        185 if operands.len() == 2 => {
            let op = FUnordLessThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        186 if operands.len() == 2 => {
            let op = FOrdGreaterThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        187 if operands.len() == 2 => {
            let op = FUnordGreaterThanOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        188 if operands.len() == 2 => {
            let op = FOrdLessThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        189 if operands.len() == 2 => {
            let op = FUnordLessThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        190 if operands.len() == 2 => {
            let op = FOrdGreaterThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        191 if operands.len() == 2 => {
            let op = FUnordGreaterThanEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        194 if operands.len() == 2 => {
            let op = ShiftRightLogicalOpBuilder::new(context)
                .base(operands[0])
                .shift(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        195 if operands.len() == 2 => {
            let op = ShiftRightArithmeticOpBuilder::new(context)
                .base(operands[0])
                .shift(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        196 if operands.len() == 2 => {
            let op = ShiftLeftLogicalOpBuilder::new(context)
                .base(operands[0])
                .shift(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        197 if operands.len() == 2 => {
            let op = BitwiseOrOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        198 if operands.len() == 2 => {
            let op = BitwiseXorOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        199 if operands.len() == 2 => {
            let op = BitwiseAndOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        200 if operands.len() == 1 => {
            let op = NotOpBuilder::new(context)
                .operand(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        201 if operands.len() == 4 => {
            let op = BitFieldInsertOpBuilder::new(context)
                .base(operands[0])
                .insert(operands[1])
                .offset(operands[2])
                .count(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        202 if operands.len() == 3 => {
            let op = BitFieldSExtractOpBuilder::new(context)
                .base(operands[0])
                .offset(operands[1])
                .count(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        203 if operands.len() == 3 => {
            let op = BitFieldUExtractOpBuilder::new(context)
                .base(operands[0])
                .offset(operands[1])
                .count(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        204 if operands.len() == 1 => {
            let op = BitReverseOpBuilder::new(context)
                .base(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        205 if operands.len() == 1 => {
            let op = BitCountOpBuilder::new(context)
                .base(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        227 if operands.len() == 3 => {
            let op = AtomicLoadOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        229 if operands.len() == 4 => {
            let op = AtomicExchangeOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        230 if operands.len() == 6 => {
            let op = AtomicCompareExchangeOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .equal(operands[2])
                .unequal(operands[3])
                .value(operands[4])
                .comparator(operands[5])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        231 if operands.len() == 6 => {
            let op = AtomicCompareExchangeWeakOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .equal(operands[2])
                .unequal(operands[3])
                .value(operands[4])
                .comparator(operands[5])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        232 if operands.len() == 3 => {
            let op = AtomicIIncrementOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        233 if operands.len() == 3 => {
            let op = AtomicIDecrementOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        234 if operands.len() == 4 => {
            let op = AtomicIAddOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        235 if operands.len() == 4 => {
            let op = AtomicISubOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        236 if operands.len() == 4 => {
            let op = AtomicSMinOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        237 if operands.len() == 4 => {
            let op = AtomicUMinOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        238 if operands.len() == 4 => {
            let op = AtomicSMaxOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        239 if operands.len() == 4 => {
            let op = AtomicUMaxOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        240 if operands.len() == 4 => {
            let op = AtomicAndOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        241 if operands.len() == 4 => {
            let op = AtomicOrOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        242 if operands.len() == 4 => {
            let op = AtomicXorOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .value(operands[3])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        318 if operands.len() == 3 => {
            let op = AtomicFlagTestAndSetOpBuilder::new(context)
                .pointer(operands[0])
                .memory(operands[1])
                .semantics(operands[2])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        400 if operands.len() == 1 => {
            let op = CopyLogicalOpBuilder::new(context)
                .operand(operands[0])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        401 if operands.len() == 2 => {
            let op = PtrEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        402 if operands.len() == 2 => {
            let op = PtrNotEqualOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        403 if operands.len() == 2 => {
            let op = PtrDiffOpBuilder::new(context)
                .operand_1(operands[0])
                .operand_2(operands[1])
                .result_type(result_type)
                .build();
            (op.id(), op.result())
        }
        _ => return None,
    })
}
