//! Evaluate the restructured program through the core interpreter, so a
//! mismatch is attributed to the pass rather than to whatever compiles its
//! output.

use tir::builtin::ModuleOp;
use tir::func::FuncOp;
use tir::interp::{self, Value};
use tir::utils::APInt;
use tir::{Context, Operation};

pub fn evaluate(context: &Context, module: &ModuleOp, arguments: &[i64]) -> Option<i64> {
    let func = module
        .body()
        .iter(context.clone())
        .find_map(|op| op.as_op::<FuncOp>())?;
    let arguments = arguments
        .iter()
        .map(|&value| Value::Int(APInt::new_signed(64, value)))
        .collect();
    interp::run_function(context, func.id(), arguments)
        .ok()?
        .first()?
        .to_i64()
}
