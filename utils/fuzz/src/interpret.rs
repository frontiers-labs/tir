//! Evaluate the restructured program through the core interpreter, so a
//! mismatch is attributed to the pass rather than to whatever compiles its
//! output.

use tir::interp::{self, Value};
use tir::utils::APInt;
use tir::{Context, OpHandle, Operation};

pub fn evaluate(context: &Context, module: &OpHandle, arguments: &[i64]) -> Option<i64> {
    let func = tir::builtin::module_body(module)
        .iter(context.clone())
        .find_map(|op| op.as_operation(&tir::func::FUNC))?;
    let arguments = arguments
        .iter()
        .map(|&value| Value::Int(APInt::new_signed(64, value)))
        .collect();
    interp::run_function(context, func.id(), arguments)
        .ok()?
        .first()?
        .to_i64()
}
