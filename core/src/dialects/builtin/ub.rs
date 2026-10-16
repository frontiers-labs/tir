//! Undefined behavior the program states: an assumption, a freeze of
//! poison, and a refinement the solver checks.

use crate::interp::{ExecutionState, Interp, InterpError, Value};
use crate::{Operation, operation};

use crate as tir;

operation! {
    AssumeOp {
        name: "assume",
        dialect: "builtin",
        scheme: "(int<1>) -> ()",
        operands: O { condition },
        requires: "condition",
        interfaces: [crate::interp::Interp],
    }
}

operation! {
    FreezeOp {
        name: "freeze",
        dialect: "builtin",
        scheme: "forall T. (T) -> T",
        operands: O { input },
        results: R { result },
        sem: "(set result (freeze input))",
        interfaces: [crate::interp::Interp, crate::Speculatable],
    }
}

// `%r = refine %x, %p`: `%r` is `%x`, and `%p` holds wherever `%x` is
// defined. The verifier proves it; see [`crate::sem::prove_refinement`].
operation! {
    RefineOp {
        name: "refine",
        dialect: "builtin",
        scheme: "forall T. (T, int<1>) -> T",
        operands: O { input, predicate },
        results: R { result },
        interfaces: [crate::Congruent, crate::interp::Interp],
    }
}

// Not speculatable: the fact holds where it was proved.
impl crate::Congruent for RefineOp {}

impl RefineOp {
    /// The value refined.
    pub fn input(&self) -> crate::ValueId {
        self.0.operands()[0]
    }

    /// What holds of it.
    pub fn predicate(&self) -> crate::ValueId {
        self.0.operands()[1]
    }
}

impl crate::Speculatable for FreezeOp {}

impl Interp for AssumeOp {
    fn evaluate(&self, _: &[Value], _: &mut ExecutionState) -> Result<Vec<Value>, InterpError> {
        Ok(Vec::new())
    }
}

impl Interp for FreezeOp {
    fn evaluate(
        &self,
        operands: &[Value],
        _: &mut ExecutionState,
    ) -> Result<Vec<Value>, InterpError> {
        let Value::Poison = operands[0] else {
            return Ok(operands.to_vec());
        };
        // Any value will do; this interpreter picks zero.
        let context = &self.handle().context;
        let ty = context.get_value(self.result()).ty();
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
            self.handle().context.type_to_string(ty)
        )))
    }
}

impl Interp for RefineOp {
    fn evaluate(
        &self,
        operands: &[Value],
        _: &mut ExecutionState,
    ) -> Result<Vec<Value>, InterpError> {
        Ok(vec![operands[0].clone()])
    }
}
