//! Verification of machine IR.

use std::collections::HashSet;

use tir::{Context, Error, OpHandle, OpId, ValueId};

use crate::backend::regalloc::RegClassId;
use crate::backend::registers::{PINS_ATTR, slot_pin};
use crate::backend::registers::{RegAssignment, register_attr, value_class};
use crate::backend::{ASSIGNMENT_ATTR, SYMBOL};

/// Verify the machine IR under `root`.
pub fn verify_machine_ir(context: &Context, root: OpId) -> Result<(), Error> {
    // Memory order is the same discipline it is in the mid-end — selection keeps
    // the chains rather than re-deriving them, so the rule that describes them
    // is the one that already did.
    crate::operation::verify_state_forks(context, root)?;
    let mut stack = vec![root];
    while let Some(op_id) = stack.pop() {
        if !context.has_operation(op_id) {
            continue;
        }
        let op = context.get_op(op_id);
        verify_instruction_ports(context, &op)?;
        verify_state_operands(context, &op)?;
        verify_slot_pins(context, &op)?;
        let symbol = op.is_operation(&SYMBOL);
        if symbol {
            verify_assignment(context, &op)?;
        }
        for region in op.regions().iter().copied() {
            for block in context.get_region(region).iter(context.clone()) {
                if symbol {
                    crate::backend::verify_block_order(context, &block)?;
                }
                stack.extend(block.op_ids());
            }
        }
    }
    Ok(())
}

fn verify_state_operands(context: &Context, op: &OpHandle) -> Result<(), Error> {
    for value in op.state_operands().iter().copied() {
        if !context.has_value(value) {
            continue;
        }
        let defined = match context.get_value(value).defining_op() {
            Some(def) => context.has_operation(def),
            None => context.is_block_argument(value),
        };
        if !defined {
            return Err(Error::VerificationError(format!(
                "{} observes %{}, a memory state nothing defines",
                op.name().as_str(),
                value.number(),
            )));
        }
    }
    Ok(())
}

fn verify_instruction_ports(context: &Context, op: &OpHandle) -> Result<(), Error> {
    // An operand or result the rest of the IR has retired names nothing: the op
    // that produced it went, and this one was not rewritten with it.
    for value in op.operands().iter().chain(op.results().iter()) {
        if !context.has_value(*value) {
            return Err(Error::VerificationError(format!(
                "{} names %{}, which no longer exists",
                op.name().as_str(),
                value.number(),
            )));
        }
    }
    let Some(instruction) = op
        .clone()
        .as_interface::<dyn crate::backend::MachineInstruction>()
    else {
        return Ok(());
    };
    let schema = crate::schema::OP_SCHEMAS
        .iter()
        .find(|s| s.dialect == op.dialect().as_str() && s.name == op.name().as_str());
    if let Some(schema) = schema {
        crate::operation::operand_fields(op, schema.name, schema.operands)?;
        crate::operation::result_fields(op, schema.name, schema.results)?;
    }
    if op.value_operands().is_empty() && op.value_results().is_empty() {
        if let Some(schema) = schema {
            for field in schema
                .attributes
                .iter()
                .filter(|a| a.ty.trim_start_matches('?') == "Register")
            {
                if register_attr(op, field.name).is_none() {
                    return Err(Error::VerificationError(format!(
                        "{} is missing register attribute '{}'",
                        op.name(),
                        field.name
                    )));
                }
            }
        }
        if let Some(syntax) = instruction.info().asm {
            for step in syntax.parse {
                if let crate::backend::asm_desc::ParseStep::Register(name, expected, _) = step {
                    let (class, index) = register_attr(op, name).ok_or(Error::ExpectedType)?;
                    if !same_view(class, *expected) || !expected.contains(index) {
                        return Err(Error::ExpectedType);
                    }
                }
            }
        }
    }

    Ok(())
}

fn verify_assignment(context: &Context, symbol: &OpHandle) -> Result<(), Error> {
    if symbol.attr(ASSIGNMENT_ATTR).is_none() {
        return Ok(());
    }
    let assignment = RegAssignment::of_op(symbol, ASSIGNMENT_ATTR);
    verify_views(context, &assignment)?;
    for value in register_values(context, symbol) {
        if assignment.get(value).is_none() {
            return Err(Error::VerificationError(format!(
                "%{} has no register in the assignment of '{}'",
                value.number(),
                match symbol.attr("name") {
                    Some(tir::attributes::AttributeValue::Str(name)) => name.to_string(),
                    _ => String::new(),
                },
            )));
        }
    }
    Ok(())
}

fn verify_slot_pins(context: &Context, op: &OpHandle) -> Result<(), Error> {
    if op.attr(PINS_ATTR).is_none() {
        return Ok(());
    }
    let Some(schema) = crate::schema::OP_SCHEMAS
        .iter()
        .find(|s| s.dialect == op.dialect().as_str() && s.name == op.name().as_str())
    else {
        return Ok(());
    };
    let inputs = crate::operation::operand_fields(op, schema.name, schema.operands)?;
    let outputs = crate::operation::result_fields(op, schema.name, schema.results)?;
    for (values, fields, mapping) in [
        (op.operands(), schema.operands, inputs),
        (op.results(), schema.results, outputs),
    ] {
        for (value, field) in values.iter().zip(mapping) {
            if let (Some((class, index)), Some(actual)) = (
                slot_pin(op, fields[field].name),
                value_class(context, *value),
            ) && !same_view(class, actual)
            {
                return Err(Error::VerificationError(format!(
                    "value %{} is pinned to {}[{index}], a different register view",
                    value.number(),
                    class.name()
                )));
            }
        }
    }
    Ok(())
}

fn verify_views(context: &Context, assignment: &RegAssignment) -> Result<(), Error> {
    for (value, (class, index)) in assignment.iter() {
        let Some(value_class) = value_class(context, value) else {
            continue;
        };
        if !same_view(class, value_class) {
            return Err(Error::VerificationError(format!(
                "%{} of class {} is pinned to {}[{}], a different register view",
                value.number(),
                value_class.name(),
                class.name(),
                index,
            )));
        }
    }
    Ok(())
}

fn same_view(a: RegClassId, b: RegClassId) -> bool {
    a.file() == b.file() && a.view.bit_offset == b.view.bit_offset
}

fn register_values(context: &Context, symbol: &OpHandle) -> Vec<ValueId> {
    let mut seen = HashSet::new();
    let mut values = Vec::new();
    let record = |value: ValueId, seen: &mut HashSet<ValueId>, values: &mut Vec<ValueId>| {
        if context.has_value(value) && value_class(context, value).is_some() && seen.insert(value) {
            values.push(value);
        }
    };
    for block_id in crate::backend::symbol_body_blocks(context, &context.get_op(symbol.id)) {
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            for value in op.value_operands().iter().chain(op.value_results().iter()) {
                record(*value, &mut seen, &mut values);
            }
        }
    }
    values
}
