use crate::utils::APInt;

use crate as tir;
use crate::{
    Commutative, ConstantLike, Context, Error, IntegerArithmetic, OpCost, SameOperandAndResultType,
    Speculatable, Subtractive,
};

tir::helpers::op_def! {
    file: "defs/converted/dialects_builtin_arith.cdsl",
    hosts: {
        constant_iface_ConstantLike => (ConstantLike, &crate::builtin::__TIR_IFACE_ConstantOp_ConstantLike),
        constant_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_ConstantOp_Interp),
        constant_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        subi_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Sub,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        subi_poison_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Splice("nsw_flag"),
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Xor,
                        arguments: &[1, 2],
                    },
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Sub,
                        arguments: &[5, 6],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Xor,
                        arguments: &[4, 7],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[3, 8],
                    },
                    tir::sem::SemRecipeStep::Splice("zero"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Lt,
                        arguments: &[9, 10],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[0, 11],
                    },
                    tir::sem::SemRecipeStep::Splice("nuw_flag"),
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::ULt,
                        arguments: &[14, 15],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[13, 16],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Or,
                        arguments: &[12, 17],
                    },
                ],
                root: 18,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "nsw_flag" => (nsw_flag)(operation, graph).into(),
                    "zero" => (zero)(operation, graph),
                    "nuw_flag" => (nuw_flag)(operation, graph).into(),
                    _ => None,
                },
            )
        },
        subi_iface_Subtractive => (Subtractive, &crate::SubtractiveCallbacks::DEFAULT),
        subi_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        subi_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        subi_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        muli_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Mul,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        muli_iface_Commutative => (Commutative, &crate::CommutativeCallbacks::DEFAULT),
        muli_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        muli_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        muli_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        muli_iface_OpCost => (OpCost, &__TIR_IFACE_MulIOp_OpCost),
        divsi_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Div,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        divsi_requires_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("zero"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Ne,
                        arguments: &[0, 1],
                    },
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Splice("smin"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Eq,
                        arguments: &[3, 4],
                    },
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("ones"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Eq,
                        arguments: &[6, 7],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[5, 8],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Not,
                        arguments: &[9],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[2, 10],
                    },
                ],
                root: 11,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "zero" => (zero)(operation, graph),
                    "smin" => (smin)(operation, graph),
                    "ones" => (ones)(operation, graph),
                    _ => None,
                },
            )
        },
        divsi_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        divsi_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        remsi_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Div,
                        arguments: &[1, 2],
                    },
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Mul,
                        arguments: &[3, 4],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Sub,
                        arguments: &[0, 5],
                    },
                ],
                root: 6,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        remsi_requires_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("zero"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Ne,
                        arguments: &[0, 1],
                    },
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Splice("smin"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Eq,
                        arguments: &[3, 4],
                    },
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("ones"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Eq,
                        arguments: &[6, 7],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[5, 8],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Not,
                        arguments: &[9],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[2, 10],
                    },
                ],
                root: 11,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "zero" => (zero)(operation, graph),
                    "smin" => (smin)(operation, graph),
                    "ones" => (ones)(operation, graph),
                    _ => None,
                },
            )
        },
        remsi_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        remsi_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        remui_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::UDiv,
                        arguments: &[1, 2],
                    },
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Mul,
                        arguments: &[3, 4],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Sub,
                        arguments: &[0, 5],
                    },
                ],
                root: 6,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        remui_requires_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("zero"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Ne,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "zero" => (zero)(operation, graph),
                    _ => None,
                },
            )
        },
        remui_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        remui_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        andi_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::And,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        andi_iface_Commutative => (Commutative, &crate::CommutativeCallbacks::DEFAULT),
        andi_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        andi_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        andi_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        ori_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Or,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        ori_iface_Commutative => (Commutative, &crate::CommutativeCallbacks::DEFAULT),
        ori_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        ori_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        ori_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        xori_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Xor,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        xori_iface_Commutative => (Commutative, &crate::CommutativeCallbacks::DEFAULT),
        xori_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        xori_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        xori_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        shli_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::ShiftLeft,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        shli_poison_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("width"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::UGe,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "width" => (width)(operation, graph),
                    _ => None,
                },
            )
        },
        shli_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        shli_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        shli_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        shrui_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::ShiftRightLogic,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        shrui_poison_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("width"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::UGe,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "width" => (width)(operation, graph),
                    _ => None,
                },
            )
        },
        shrui_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        shrui_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        shrui_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        shrsi_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::ShiftRightArithmetic,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        shrsi_poison_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Splice("width"),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::UGe,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "width" => (width)(operation, graph),
                    _ => None,
                },
            )
        },
        shrsi_iface_SameOperandAndResultType => (SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        shrsi_iface_IntegerArithmetic => (IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        shrsi_iface_Speculatable => (Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        cmpi_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("cmp_expr")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "cmp_expr" => (crate::builtin::cmpi_cmp_expr)(operation, graph),
                    _ => None,
                },
            )
        },
        cmpi_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        extsi_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::ResultWidth {
                        significant_bits: false,
                        subtract: 0,
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::SExt,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        extsi_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        extui_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::ResultWidth {
                        significant_bits: false,
                        subtract: 0,
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::ZExt,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        extui_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        trunci_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::ResultWidth {
                        significant_bits: false,
                        subtract: 1,
                    },
                    tir::sem::SemRecipeStep::Integer {
                        width: 16,
                        value: 0,
                        signed: false,
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Extract,
                        arguments: &[0, 1, 2],
                    },
                ],
                root: 3,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        trunci_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        bitcast_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Bitcast,
                        arguments: &[0],
                    },
                ],
                root: 1,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        bitcast_verifier => crate::builtin::bitcast_verify_impl,
        bitcast_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
    },
}

pub(crate) fn constant_constantlike_constant_value(operation: &tir::OpHandle) -> tir::utils::APInt {
    let context = operation.context.clone();
    let ty = context
        .get_value(crate::builtin::constant_result(operation))
        .ty();
    let width = crate::builtin::IntegerType::view(&context, ty).map_or(64, |int| int.width());
    operation
        .attr("value")
        .and_then(|value| value.int_at(width))
        .unwrap_or_else(|| tir::utils::APInt::zero(width).with_signed(true))
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_ConstantOp_ConstantLike: crate::ConstantLikeCallbacks =
    crate::ConstantLikeCallbacks {
        constant_value: crate::builtin::constant_constantlike_constant_value,
    };

/// A `builtin.constant` of `ty` holding `value`, wider than 64 bits or not.
pub fn literal(
    context: &Context,
    value: &tir::utils::APInt,
    ty: tir::TypeId,
    parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> crate::OpHandle {
    r#constant(
        context,
        tir::attributes::AttributeValue::from_int(value),
        ty,
        parts,
        insertion,
    )
}

// The rest of an invocation is more fields of the op: its flags, and where
// it is poison or must not run.

fn at_width(
    operation: &tir::OpHandle,
    g: &mut tir::sem::SemGraph,
    value: fn(u64) -> APInt,
) -> Option<tir::NodeId> {
    let context = &operation.context;
    let ty = context.get_value(operation.operands()[0]).ty();
    let width = crate::builtin::IntegerType::view(context, ty)?.width();
    let node = g.add_node(crate::sem::SymKind::Constant);
    g.set_leaf_data(node, crate::sem::SymPayload::Int(value(width)));
    Some(node)
}

fn zero(operation: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
    at_width(operation, g, APInt::zero)
}

fn width(operation: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
    at_width(operation, g, |width| APInt::new(width, width))
}

fn smin(operation: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
    at_width(operation, g, |width| APInt::new(width, 1).shl(width - 1))
}

fn ones(operation: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
    at_width(operation, g, |width| APInt::zero(width).not())
}

fn flag(operation: &tir::OpHandle, g: &mut tir::sem::SemGraph, name: &str) -> tir::NodeId {
    let set = matches!(
        operation.attr(name),
        Some(tir::attributes::AttributeValue::Bool(true))
    );
    crate::sem::con(g, u64::from(set), 1)
}

fn nsw_flag(operation: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> tir::NodeId {
    flag(operation, g, "nsw")
}

fn nuw_flag(operation: &tir::OpHandle, g: &mut tir::sem::SemGraph) -> tir::NodeId {
    flag(operation, g, "nuw")
}

tir::helpers::op_def! {
    file: "defs/builtin-arith.cdsl",
    hosts: {
        ops::zero => zero,
        ops::Additive => (crate::Additive, &crate::AdditiveCallbacks::DEFAULT),
        ops::Commutative => (crate::Commutative, &crate::CommutativeCallbacks::DEFAULT),
        ops::SameOperandAndResultType => (crate::SameOperandAndResultType, &crate::SameOperandAndResultTypeCallbacks::DEFAULT),
        ops::IntegerArithmetic => (crate::IntegerArithmetic, &crate::IntegerArithmeticCallbacks::DEFAULT),
        ops::Speculatable => (crate::Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
    },
}

// Remainder is defined by the Euclidean identity rather than a primitive
// srem/urem, so the semantic form matches the canonical sub-mul-div target
// that TMDL rem/remu behaviors reduce to and selects through the e-graph
// without an unprovable rewrite. This total form equals bvsrem/bvurem
// everywhere, including rhs=0 (a - x*0 = a) and MIN/-1 (0). IR-level
// partiality (C's UB at rhs=0) is unchanged.

pub(crate) fn muli_opcost_cost(_operation: &tir::OpHandle) -> u32 {
    4
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_MulIOp_OpCost: crate::OpCostCallbacks = crate::OpCostCallbacks {
    cost: Some(crate::builtin::muli_opcost_cost),
};

pub(crate) fn cmpi_cmp_expr(
    operation: &tir::OpHandle,
    g: &mut tir::sem::SemGraph,
) -> Option<tir::NodeId> {
    let tir::attributes::AttributeValue::Predicate(predicate) = operation.attr("predicate")? else {
        return None;
    };
    compare_expr(g, predicate, false)
}

pub(crate) fn compare_expr(
    g: &mut tir::sem::SemGraph,
    predicate: tir::attributes::Predicate,
    unsigned_only: bool,
) -> Option<tir::NodeId> {
    use tir::attributes::Predicate;
    use tir::sem::SymKind;

    let swap = matches!(
        predicate,
        Predicate::Sgt | Predicate::Sle | Predicate::Ugt | Predicate::Ule
    );
    let kind = match if swap { predicate.swapped() } else { predicate } {
        Predicate::Eq => SymKind::Eq,
        Predicate::Ne => SymKind::Ne,
        Predicate::Slt if !unsigned_only => SymKind::Lt,
        Predicate::Sge if !unsigned_only => SymKind::Ge,
        Predicate::Ult => SymKind::ULt,
        Predicate::Uge => SymKind::UGe,
        _ => return None,
    };

    let mut operand = |index: u32| {
        let leaf = g.add_node(SymKind::Symbol);
        g.set_leaf_data(leaf, tir::sem::SymPayload::SymbolId(index));
        leaf
    };
    let (lhs, rhs) = if swap {
        (operand(1), operand(0))
    } else {
        (operand(0), operand(1))
    };
    let node = g.add_node(kind);
    g.add_edge(node, lhs);
    g.add_edge(node, rhs);
    Some(node)
}

pub(crate) fn bitcast_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    // An index is a scalar too, but only a data layout says how wide.
    let layout = crate::DataLayout::for_op(context, operation.id);
    let width = |value| {
        let ty = context.get_value(value).ty();
        crate::builtin::IntegerType::view(context, ty)
            .map(|int| int.width())
            .or_else(|| {
                crate::builtin::FloatType::view(context, ty)
                    .map(|float| u64::from(float.bit_width()))
            })
            .or_else(|| {
                crate::builtin::IndexType::view(context, ty)
                    .and_then(|_| layout.as_ref()?.index_width().map(u64::from))
            })
    };
    let input_width = width(operation.operands()[0]);
    let result_width = width(crate::builtin::bitcast_result(operation));
    if input_width.is_none() || result_width.is_none() {
        return Err(Error::VerificationError(
            "bitcast requires scalar integer, floating-point or index types".to_string(),
        ));
    }
    if input_width != result_width {
        return Err(Error::VerificationError(
            "bitcast source and result widths must match".to_string(),
        ));
    }
    Ok(())
}
