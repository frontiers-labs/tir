//! A reference evaluator for the small IR the restructuring fuzz target
//! generates: integer arithmetic, comparisons, and the structured operations
//! restructuring produces. It exists so a mismatch can be attributed to the
//! pass rather than to whatever compiles its output.

use std::collections::HashMap;

use tir::attributes::{AttributeValue, Predicate};
use tir::builtin::ModuleOp;
use tir::func::FuncOp;
use tir::{Context, OpId, Operation, RegionId, ValueId};

pub fn evaluate(context: &Context, module: &ModuleOp, arguments: &[i64]) -> Option<i64> {
    let func = module
        .body()
        .iter(context.clone())
        .find_map(|op| op.as_op::<FuncOp>())?;
    let region = func.regions().next()?;
    let block = region.iter(context.clone()).next()?;
    let mut values: HashMap<ValueId, i64> = block
        .arguments()
        .iter()
        .map(|argument| argument.id())
        .zip(arguments.iter().copied())
        .collect();
    match run(context, block.op_ids(), &mut values)? {
        Leave::Return(result) => result.first().copied(),
        _ => None,
    }
}

enum Leave {
    Return(Vec<i64>),
    Yield(Vec<i64>),
    /// `scf.condition`: whether to iterate again, and the values carried out.
    Condition(bool, Vec<i64>),
}

fn run(context: &Context, ops: Vec<OpId>, values: &mut HashMap<ValueId, i64>) -> Option<Leave> {
    for op in ops {
        let instance = context.get_op(op);
        let named = |dialect: &str, name: &str| {
            instance.dialect().as_str() == dialect && instance.name().as_str() == name
        };
        let read = |index: usize| values.get(&instance.operands()[index]).copied();
        let computed = if named("builtin", "constant") {
            match instance.attr("value")? {
                AttributeValue::Int(value) => Some(value),
                _ => None,
            }
        } else if named("builtin", "addi") {
            Some(read(0)?.wrapping_add(read(1)?))
        } else if named("builtin", "subi") {
            Some(read(0)?.wrapping_sub(read(1)?))
        } else if named("builtin", "muli") {
            Some(read(0)?.wrapping_mul(read(1)?))
        } else if named("builtin", "cmpi") {
            let (left, right) = (read(0)?, read(1)?);
            let AttributeValue::Predicate(predicate) = instance.attr("predicate")? else {
                return None;
            };
            Some(i64::from(match predicate {
                Predicate::Eq => left == right,
                Predicate::Ne => left != right,
                Predicate::Slt => left < right,
                Predicate::Sgt => left > right,
                Predicate::Sle => left <= right,
                Predicate::Sge => left >= right,
                _ => return None,
            }))
        } else {
            None
        };
        if let Some(computed) = computed {
            values.insert(instance.results()[0], computed);
            continue;
        }

        if named("func", "return") {
            return Some(Leave::Return(operands(&instance.operands(), values)?));
        }
        if named("scf", "yield") {
            return Some(Leave::Yield(operands(&instance.operands(), values)?));
        }
        if named("scf", "condition") {
            let repeat = read(0)? != 0;
            return Some(Leave::Condition(
                repeat,
                operands(&instance.operands()[1..], values)?,
            ));
        }

        let results = if named("scf", "if") {
            let taken = usize::from(read(0)? == 0);
            let Leave::Yield(results) = region(context, instance.regions()[taken], &[], values)?
            else {
                return None;
            };
            results
        } else if named("scf", "switch") {
            let predicate = read(0)?;
            let cases = match instance.attr("cases")? {
                AttributeValue::Array(cases) => cases
                    .iter()
                    .map(|case| match case {
                        AttributeValue::Int(value) => Some(*value),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?,
                _ => return None,
            };
            let taken = cases
                .iter()
                .position(|case| *case == predicate)
                .unwrap_or(cases.len());
            let Leave::Yield(results) = region(context, instance.regions()[taken], &[], values)?
            else {
                return None;
            };
            results
        } else if named("scf", "while") {
            let mut carried = operands(&instance.operands(), values)?;
            loop {
                let Leave::Condition(repeat, next) =
                    region(context, instance.regions()[0], &carried, values)?
                else {
                    return None;
                };
                carried = next;
                if !repeat {
                    break carried;
                }
            }
        } else {
            return None;
        };
        for (&result, value) in instance.results().iter().zip(results) {
            values.insert(result, value);
        }
    }
    None
}

/// Run a region's only block with `arguments` bound to its entry arguments.
/// The block sees the enclosing values too: an `scf` region is not isolated.
fn region(
    context: &Context,
    region: RegionId,
    arguments: &[i64],
    values: &mut HashMap<ValueId, i64>,
) -> Option<Leave> {
    let block = context.get_region(region).iter(context.clone()).next()?;
    let mut inner = values.clone();
    for (argument, value) in block.arguments().iter().zip(arguments) {
        inner.insert(argument.id(), *value);
    }
    run(context, block.op_ids(), &mut inner)
}

fn operands(operands: &[ValueId], values: &HashMap<ValueId, i64>) -> Option<Vec<i64>> {
    operands
        .iter()
        .map(|operand| values.get(operand).copied())
        .collect()
}
