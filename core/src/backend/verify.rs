//! Verification of machine IR.
//!
//! Machine IR is not SSA — block-parameter destruction leaves a parameter
//! defined once per predecessor — so the generic op-tree verifier does not
//! describe it. What it does have is one register notation, and these are its
//! rules:
//!
//! - an instruction names only values that still exist;
//! - every register slot of an instruction holds a value or a register, never
//!   neither: slots are read off in port order;
//! - a register slot holding a value must hold one whose class views the same
//!   register file at the same bit offset: a narrower class of that view is an
//!   allocation constraint, a different view is a type error;
//! - a register assignment must place a value in a register of a class sharing
//!   its own view;
//! - once a function carries an assignment, it is total: every register-typed
//!   value some instruction names has an entry;
//! - a symbol's own blocks are ordered by their dependences. Only those: a
//!   module or a section lists definitions, and a definition is reachable from
//!   the whole module however the list is spelled, so a caller may stand ahead
//!   of the λ it names.

use std::collections::HashSet;

use tir::{Context, Error, OpHandle, OpId, ValueId};

use crate::backend::regalloc::RegClassId;
use crate::backend::registers::{PINS_ATTR, slot_pin};
use crate::backend::registers::{RegAssignment, RegSlot, reg_slots, value_class};
use crate::backend::{ARG_PINS_ATTR, ASSIGNMENT_ATTR, SymbolOp};

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
        verify_reg_slots(context, &op)?;
        verify_state_operands(context, &op)?;
        verify_views(context, &RegAssignment::of_op(&op, ARG_PINS_ATTR))?;
        verify_slot_pins(context, &op)?;
        let symbol = op.is::<SymbolOp>();
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

/// Every memory state an operation observes is one something still names: an
/// operation that is still here, or a block parameter standing for the chain the
/// block is entered on. A port naming a definition selection took away is an
/// edge to nothing, and these edges are the memory order from here to encoding.
fn verify_state_operands(context: &Context, op: &OpHandle) -> Result<(), Error> {
    let state = tir::builtin::StateType::new(context);
    for value in op.operands().iter().copied() {
        if !context.has_value(value) || context.get_value(value).ty() != state {
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

fn verify_reg_slots(context: &Context, op: &OpHandle) -> Result<(), Error> {
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
    let slots = reg_slots(op);
    // A slot is an SSA position or an attribute, and every port has one:
    // positions are read off in port order, so a port with neither would be
    // read as the next port's.
    for port in crate::backend::reg_ports(op) {
        if op.attr(port.name).is_some() || slots.iter().any(|slot| slot.port.name == port.name) {
            continue;
        }
        return Err(Error::VerificationError(format!(
            "{} register slot '{}' holds neither a value nor a register",
            op.name().as_str(),
            port.name,
        )));
    }
    // Every SSA position is some port's: a surplus operand or result would be
    // read by nothing, and a missing one shifts every later port. The trailing
    // `!state` ports are memory order, not registers, and are not counted.
    let (values_read, values_written) = slots.iter().fold((0, 0), |(read, written), slot| {
        match (slot.slot, slot.port.def) {
            (RegSlot::Value(_), false) => (read + 1, written),
            (RegSlot::Value(_), true) => (read, written + 1),
            _ => (read, written),
        }
    });
    let state = tir::builtin::StateType::new(context);
    let registers = |values: &[ValueId]| {
        values
            .iter()
            .filter(|value| !context.has_value(**value) || context.get_value(**value).ty() != state)
            .count()
    };
    let (operands, results) = (registers(&op.operands()), registers(&op.results()));
    if !crate::backend::reg_ports(op).is_empty()
        && (values_read != operands || values_written != results)
    {
        return Err(Error::VerificationError(format!(
            "{} has {} operands and {} results for {} use and {} def register slots",
            op.name().as_str(),
            operands,
            results,
            values_read,
            values_written,
        )));
    }
    for slot in &slots {
        let (RegSlot::Value(value), Some(port_class)) = (slot.slot, slot.port.class) else {
            continue;
        };
        let Some(class) = value_class(context, value) else {
            return Err(Error::VerificationError(format!(
                "{} operand '{}' reads %{}, which is not a register",
                op.name().as_str(),
                slot.port.name,
                value.number(),
            )));
        };
        if !same_view(class, port_class) {
            return Err(Error::VerificationError(format!(
                "{} operand '{}' reads %{} of class {} through {}, a different register view",
                op.name().as_str(),
                slot.port.name,
                value.number(),
                class.name(),
                port_class.name(),
            )));
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

/// A slot may only be pinned to a register of a class sharing the view of the
/// value it holds.
fn verify_slot_pins(context: &Context, op: &OpHandle) -> Result<(), Error> {
    if op.attr(PINS_ATTR).is_none() {
        return Ok(());
    }
    for slot in reg_slots(op) {
        let (RegSlot::Value(value), Some((class, index))) =
            (slot.slot, slot_pin(op, slot.port.name))
        else {
            continue;
        };
        let Some(value_class) = value_class(context, value) else {
            continue;
        };
        if !same_view(class, value_class) {
            return Err(Error::VerificationError(format!(
                "{} slot '{}' holds %{} of class {} but is pinned to {}[{}]",
                op.name().as_str(),
                slot.port.name,
                value.number(),
                value_class.name(),
                class.name(),
                index,
            )));
        }
    }
    Ok(())
}

/// A value may only be pinned to, or assigned, a register of a class sharing
/// its own architectural view.
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

/// Whether two classes view the same register file at the same bit offset. A
/// register group read through a single-register slot — an RVV LMUL group in a
/// `VR` operand — is the same file at the same offset, and is the allocation
/// unit rather than a different view, so group width does not enter.
fn same_view(a: RegClassId, b: RegClassId) -> bool {
    a.file() == b.file() && a.view.bit_offset == b.view.bit_offset
}

/// Every register-typed value the symbol's instructions name. A block parameter
/// no instruction names needs no register: its predecessors' copies were
/// rewritten away with it.
fn register_values(context: &Context, symbol: &OpHandle) -> Vec<ValueId> {
    let mut seen = HashSet::new();
    let mut values = Vec::new();
    let record = |value: ValueId, seen: &mut HashSet<ValueId>, values: &mut Vec<ValueId>| {
        if context.has_value(value) && value_class(context, value).is_some() && seen.insert(value) {
            values.push(value);
        }
    };
    for region in context.get_op(symbol.id).regions().iter().copied() {
        for block in context.get_region(region).iter(context.clone()) {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                for value in op.operands().iter().chain(op.results().iter()) {
                    record(*value, &mut seen, &mut values);
                }
            }
        }
    }
    values
}
