//! Stack-slot classification: which allocations a pass may treat as values.
//!
//! Reading the slots is the question every memory transform asks first — scalar
//! promotion, state threading and the view's memory laws all need to know which
//! allocation a location names and whether its address is observable elsewhere.
//! The reading is interface-driven (`PromotableAllocation`, `MemoryRead`,
//! `MemoryWrite`), so it says nothing about which dialect spells the slot.

use std::collections::BTreeMap;

use crate::analysis::{Escape, EscapeFacts, access_of};
use crate::ptr::PtrAddOp;
use crate::{Context, OpId, PromotableAllocation, TypeId, ValueId};

/// What is known about one allocated stack slot across the operations it was
/// collected from.
#[derive(Default)]
pub struct SlotState {
    /// The allocation that opens the slot, if it was among the collected ops.
    pub alloca: Option<OpId>,
    /// Every write into the slot, whether it names the slot itself or a pointer
    /// derived from it.
    pub stores: Vec<OpId>,
    /// Every read of the slot, named the same two ways.
    pub loads: Vec<OpId>,
    /// The slot's address travels somewhere the analysis cannot follow, so its
    /// contents may be observed indirectly and it cannot be promoted.
    pub escapes: bool,
}

/// Collect every load and store in `op_ids` against the slots opened by the
/// `PromotableAllocation`s among them, reading each slot's escape verdict off
/// `escapes`.
///
/// `op_ids` bounds the collection: a slot is only correctly described when every
/// operation that could name its pointer is included. The escape verdict is not
/// bounded that way — it is a fact about the whole function.
pub fn collect_slots(
    context: &Context,
    escapes: &EscapeFacts,
    op_ids: &[OpId],
) -> BTreeMap<ValueId, SlotState> {
    let mut slots: BTreeMap<ValueId, SlotState> = BTreeMap::new();

    for &op_id in op_ids {
        if let Some(allocation) = context
            .get_op(op_id)
            .as_interface::<dyn PromotableAllocation>()
        {
            let pointer = allocation.promoted_location();
            let slot = slots.entry(pointer).or_default();
            slot.alloca = Some(op_id);
            slot.escapes = escapes.escape(pointer) != Escape::Local;
        }
    }

    for &op_id in op_ids {
        let instance = context.get_op(op_id);
        if instance.has_interface::<dyn PromotableAllocation>() {
            continue;
        }
        let Some(access) = access_of(&instance) else {
            continue;
        };
        let Some(slot) = slots.get_mut(&slot_base(context, access.location)) else {
            continue;
        };
        match access.write {
            true => slot.stores.push(op_id),
            false => slot.loads.push(op_id),
        }
    }

    slots
}

fn slot_base(context: &Context, pointer: ValueId) -> ValueId {
    let mut base = pointer;
    while let Some(defining) = context.get_value(base).defining_op() {
        let instance = context.get_op(defining);
        if !instance.is::<PtrAddOp>() {
            break;
        }
        base = instance.operands()[0];
    }
    base
}

/// The one type the slot's loads and stores name, or `None` where they disagree.
/// Promotion carries a single value per slot and substitutes it for the loads,
/// so a slot whose accesses disagree on type — a frontend spelling one pointer
/// both opaque and typed, say — has no type to give that value and stays in
/// memory. A slot nothing accesses has no type either, and nothing to carry.
pub fn agreed_value_type(context: &Context, state: &SlotState) -> Option<TypeId> {
    let mut types = state.loads.iter().chain(&state.stores).map(|&op| {
        let named = access_of(&context.get_op(op)).expect("a collected access names a value");
        context.get_value(named.value).ty()
    });
    let ty = types.next()?;
    types.all(|other| other == ty).then_some(ty)
}
