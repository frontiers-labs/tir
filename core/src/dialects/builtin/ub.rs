//! Undefined behavior the program states: an assumption, a freeze of
//! poison, and a refinement the solver checks.

use crate::Operation;
use crate::interp::{ExecutionState, InterpError, Value};

use crate as tir;

tir::helpers::op_def! {
    file: "defs/converted/dialects_builtin_ub.cdsl",
    hosts: {
        assume_requires_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Operand(0)],
                root: 0,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        assume_iface_Interp => (crate :: interp :: Interp, &crate::builtin::__TIR_IFACE_AssumeOp_Interp),
        freeze_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Freeze,
                        arguments: &[0],
                    },
                ],
                root: 1,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        freeze_iface_Interp => (crate :: interp :: Interp, &crate::builtin::__TIR_IFACE_FreezeOp_Interp),
        freeze_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        refine_iface_Congruent => (crate :: Congruent, &crate::CongruentCallbacks::DEFAULT),
        refine_iface_Interp => (crate :: interp :: Interp, &crate::builtin::__TIR_IFACE_RefineOp_Interp),
    },
}

// `%r = refine %x, %p`: `%r` is `%x`, and `%p` holds wherever `%x` is
// defined. The verifier proves it; see the semantic refinement checker.

// Not speculatable: the fact holds where it was proved.

/// The value refined.
pub fn refine_input(operation: &tir::OpHandle) -> crate::ValueId {
    operation.operands()[0]
}

/// What holds of it.
pub fn refine_predicate(operation: &tir::OpHandle) -> crate::ValueId {
    operation.operands()[1]
}

pub(crate) fn assume_interp_evaluate(
    __operation: &tir::OpHandle,
    _: &[Value],
    _: &mut ExecutionState,
) -> Result<Vec<Value>, InterpError> {
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_AssumeOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::builtin::assume_interp_evaluate,
    };

pub(crate) fn freeze_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[Value],
    _: &mut ExecutionState,
) -> Result<Vec<Value>, InterpError> {
    let Value::Poison = operands[0] else {
        return Ok(operands.to_vec());
    };
    // Any value will do; this interpreter picks zero.
    let context = &operation.handle().context;
    let ty = context
        .get_value(crate::builtin::freeze_result(operation))
        .ty();
    let int = |width| Value::Int(tir_adt::APInt::new(width, 0));
    if let Some(integer) = crate::builtin::IntegerType::view(context, ty) {
        return Ok(vec![int(integer.width())]);
    }
    if let Some(float) = crate::builtin::FloatType::view(context, ty) {
        let zero = tir_adt::APFloat::from_bits(float.exp_width(), float.mant_width(), false, 0);
        return Ok(vec![Value::Float(zero)]);
    }
    Err(InterpError::Message(format!(
        "builtin.freeze of poison {}",
        operation.handle().context.type_to_string(ty)
    )))
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_FreezeOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::builtin::freeze_interp_evaluate,
    };

pub(crate) fn refine_interp_evaluate(
    __operation: &tir::OpHandle,
    operands: &[Value],
    _: &mut ExecutionState,
) -> Result<Vec<Value>, InterpError> {
    Ok(vec![operands[0].clone()])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_RefineOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::builtin::refine_interp_evaluate,
    };
