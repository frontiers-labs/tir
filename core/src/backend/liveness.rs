//! The register classes each virtual register of a machine function is
//! referenced through, and what allocating it from all of them at once
//! leaves.
//!
//! Register operands are SSA operands and results, read through [`op_regs`]
//! (see [`crate::analysis::defuse`]); a virtual register is a value, named here
//! by its value number.

use std::collections::{BTreeSet, HashMap};

use tir::backend::regalloc::RegClassId;
use tir::{BlockId, Context, ValueId};

pub use crate::analysis::defuse::{OpRegs, PhysReg, execution_regs, op_regs};

use crate::backend::registers::value_class;

/// What the classes a function references its virtual registers through
/// constrain them to.
#[derive(Debug, Default)]
pub struct Classes {
    /// The architectural view each virtual register is allocated through: the
    /// narrowest of every class it is referenced by (see
    /// [`RegClassId::is_subclass_of`]). Governs the width of the copies and spill
    /// code the allocator emits for it.
    pub vreg_class: HashMap<u32, RegClassId>,
    /// The file indices a virtual register may be assigned: the intersection of
    /// the register sets of every class it is referenced through. Absent means
    /// unconstrained beyond [`Classes::vreg_class`].
    pub allowed_indices: HashMap<u32, BTreeSet<u16>>,
    /// Virtual registers referenced through classes that cannot both be honored
    /// (different files or views, or no register in common), with the pair.
    pub class_conflicts: HashMap<u32, (RegClassId, RegClassId)>,
}

impl Classes {
    /// The classes every value of `blocks` is referenced through: its type and
    /// the slots naming it.
    pub fn of(context: &Context, blocks: &[BlockId]) -> Self {
        let mut result = Self::default();
        let mut seen: HashMap<ValueId, Option<RegClassId>> = HashMap::new();
        for &block in blocks {
            for param in context.get_block(block).value_arguments() {
                record_class(&mut result, context, param.id(), None, &mut seen);
            }
            for op_id in context.get_block(block).op_ids() {
                let op = context.get_op(op_id);
                let slots = crate::backend::reg_slots(&op);
                let regs = crate::analysis::defuse::op_regs_from(&op, &slots);
                let port_classes = slot_classes(&slots);
                for &value in regs.uses.iter().chain(&regs.defs) {
                    if context.has_value(value) {
                        let port = slot_class(&port_classes, value);
                        record_class(&mut result, context, value, port, &mut seen);
                    }
                }
            }
        }
        result
    }
}

fn slot_classes(slots: &[crate::backend::SlotRef]) -> Vec<(ValueId, RegClassId)> {
    let mut classes = Vec::with_capacity(slots.len());
    for slot in slots {
        if let (crate::backend::RegSlot::Value(value), Some(class)) = (slot.slot, slot.port.class)
            && !classes.iter().any(|(seen, _)| *seen == value)
        {
            classes.push((value, class));
        }
    }
    classes
}

fn slot_class(classes: &[(ValueId, RegClassId)], value: ValueId) -> Option<RegClassId> {
    classes
        .iter()
        .find(|(slot, _)| *slot == value)
        .map(|(_, class)| *class)
}

fn record_class(
    result: &mut Classes,
    context: &Context,
    value: ValueId,
    port_class: Option<RegClassId>,
    seen: &mut HashMap<ValueId, Option<RegClassId>>,
) {
    let id = value.number();
    // A value's own class is its type, which does not change while the scan
    // runs; reading it goes through the type interner, so read it once.
    let own = *seen
        .entry(value)
        .or_insert_with(|| value_class(context, value));
    for class in own.iter().chain(port_class.iter()) {
        record_one_class(result, id, *class);
    }
}

fn record_one_class(result: &mut Classes, id: u32, class: RegClassId) {
    let Some(current) = result.vreg_class.get(&id).copied() else {
        result.vreg_class.insert(id, class);
        result
            .allowed_indices
            .insert(id, class.registers.iter().copied().collect());
        return;
    };
    // The allowed set is within the current class's registers already.
    if class == current {
        return;
    }
    // A register group named through a single-register slot (an RVV LMUL group
    // in a `VR` operand) allocates as the group: the wider class decides, and
    // the narrower one constrains nothing.
    if class.group_width != current.group_width && class.file() == current.file() {
        if class.group_width > current.group_width {
            result.vreg_class.insert(id, class);
            result
                .allowed_indices
                .insert(id, class.registers.iter().copied().collect());
        }
        return;
    }
    if !class.shares_view_with(current) {
        result.class_conflicts.entry(id).or_insert((current, class));
        return;
    }
    let allowed = result.allowed_indices.entry(id).or_default();
    allowed.retain(|index| class.contains(*index));
    if allowed.is_empty() {
        result.class_conflicts.entry(id).or_insert((current, class));
    } else if class.is_subclass_of(current) && !current.is_subclass_of(class) {
        result.vreg_class.insert(id, class);
    }
}
