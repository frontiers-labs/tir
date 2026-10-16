//! Which object every pointer names.
//!
//! A pointer reads back to the object it was derived from — a stack
//! allocation, a global, a parameter — through the `ptradd` and `ptr.field`
//! chains and the spill slots a frontend leaves behind. Distinct objects never name the same
//! memory, and neither does an allocation whose address never leaves its own
//! accesses.
//!
//! Objects are whole: the walk is field-insensitive, flow-insensitive, and
//! says nothing about what a call does to memory.

use std::collections::HashSet;

use crate::builtin::GlobalOp;
use crate::func::FuncOp;
use crate::ptr::{FieldOp, PtrAddOp};
use crate::{Context, MemoryRead, MemoryWrite, PromotableAllocation, RegionId, Value, ValueId};

/// The object a pointer was derived from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Base {
    /// A stack allocation, named by the pointer it opens.
    Alloca(ValueId),
    /// A global, named by the module-level value declaring it.
    Global(ValueId),
    /// A pointer the function was entered with. `noalias` where the caller
    /// guarantees nothing else the function reaches names that memory —
    /// `restrict` in C.
    Param { pointer: ValueId, noalias: bool },
}

impl Base {
    /// Whether two objects are known to be different memory. Two parameters, or
    /// a parameter and a global, may name the same memory; a stack allocation
    /// is fresh and a `noalias` parameter is guaranteed unaliased, so nothing
    /// else is either.
    pub fn distinct(self, other: Base) -> bool {
        self != other
            && (self.unshared()
                || other.unshared()
                || matches!((self, other), (Base::Global(_), Base::Global(_))))
    }

    /// Whether the object is nothing else the function names.
    fn unshared(self) -> bool {
        matches!(self, Base::Alloca(_) | Base::Param { noalias: true, .. })
    }
}

/// The object `address` is derived from through pointer arithmetic: a stack
/// allocation, a global, or a parameter of the function, and nothing else.
///
/// Read off the IR directly, so the converter can ask it while the function is
/// still a graph of blocks and its parameters are the entry block's arguments.
pub fn object_base(context: &Context, address: ValueId) -> Option<Base> {
    let mut seen = HashSet::new();
    let mut current = address;
    loop {
        // A slot whose one store is derived from a load of itself — a pointer a
        // loop advances — reads back to where the walk already was, and names no
        // object outside it.
        if !seen.insert(current) {
            return None;
        }
        let Some(op) = context.get_value(current).defining_op() else {
            let region = parameter_region(context, current)?;
            let function = context.get_region(region).parent_op()?;
            let function = context.get_op(function);
            let function = function.as_op::<FuncOp>()?;
            let noalias = function.noalias_arguments().into_iter().any(|index| {
                context.get_region(region).ports().get(index).map(Value::id) == Some(current)
            });
            return Some(Base::Param {
                pointer: current,
                noalias,
            });
        };
        if !context.has_operation(op) {
            return None;
        }
        let instance = context.get_op(op);
        if instance.is::<PtrAddOp>() || instance.is::<FieldOp>() {
            current = instance.operands()[0];
        } else if instance.has_interface::<dyn PromotableAllocation>() {
            return Some(Base::Alloca(current));
        } else if instance.is::<GlobalOp>() {
            return Some(Base::Global(current));
        } else if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>()
            && read.read_value() == current
            && let Some(held) = only_pointer_held(context, read.read_location())
        {
            current = held;
        } else {
            return None;
        }
    }
}

/// The one pointer a slot ever holds, where reading it back can only give that
/// pointer: a fresh allocation whose address never leaves its own accesses and
/// which exactly one write, of the whole slot, ever names. That is what a
/// frontend spilling a pointer parameter it never assigns again leaves behind,
/// and reading through the spill is how the object the parameter names reaches
/// the accesses derived from it.
fn only_pointer_held(context: &Context, slot: ValueId) -> Option<ValueId> {
    let allocation = context.get_value(slot).defining_op()?;
    if !context
        .get_op(allocation)
        .has_interface::<dyn PromotableAllocation>()
    {
        return None;
    }
    let mut held = None;
    for user in context.users_of(slot) {
        let instance = context.get_op(user);
        let write = instance.clone().as_interface::<dyn MemoryWrite>();
        let location = write
            .as_ref()
            .map(|write| write.write_location())
            .or_else(|| {
                instance
                    .clone()
                    .as_interface::<dyn MemoryRead>()
                    .map(|read| read.read_location())
            });
        if location != Some(slot) || instance.operands().iter().filter(|&&v| v == slot).count() != 1
        {
            return None;
        }
        if let Some(write) = write {
            if held.is_some() {
                return None;
            }
            held = Some(write.written_value());
        }
    }
    held.filter(|_| accessed_only(context, slot))
}

/// The region `value` is a parameter of: its own port, or an argument of the
/// entry block a region is entered on, which is the same list either way.
fn parameter_region(context: &Context, value: ValueId) -> Option<RegionId> {
    if let Some(region) = context.region_of_port(value) {
        return Some(region);
    }
    let block = context.block_of_argument(value)?;
    let region = context.parent_region(block)?;
    (context.get_region(region).entry_block() == block).then_some(region)
}

/// Whether every use of `address`, through pointer arithmetic, is as the
/// location of a read or a write: the address itself never leaves the
/// function's own accesses.
///
/// Pointer arithmetic branches and rejoins, so the uses form a DAG; a pointer
/// two derivations reach is one to answer once, not once per path. The values
/// a region result names are read once for the same reason: that list is the
/// one place a use does not appear in, and finding it walks the region tree.
pub fn accessed_only(context: &Context, address: ValueId) -> bool {
    let published: HashSet<ValueId> = crate::region::defining_region(context, address)
        .map(|region| {
            context
                .nested_regions(region)
                .iter()
                .flat_map(|&nested| context.get_region(nested).results())
                .collect()
        })
        .unwrap_or_default();
    accessed_only_seen(context, address, &published, &mut HashSet::new())
}

fn accessed_only_seen(
    context: &Context,
    address: ValueId,
    published: &HashSet<ValueId>,
    seen: &mut HashSet<ValueId>,
) -> bool {
    if published.contains(&address) || !seen.insert(address) {
        return !published.contains(&address);
    }
    context.users_of(address).into_iter().all(|user| {
        let instance = context.get_op(user);
        if instance.is::<PtrAddOp>() || instance.is::<FieldOp>() {
            return instance.operands()[0] == address
                && instance
                    .results()
                    .to_vec()
                    .into_iter()
                    .all(|derived| accessed_only_seen(context, derived, published, seen));
        }
        let location = instance
            .clone()
            .as_interface::<dyn MemoryWrite>()
            .map(|write| write.write_location())
            .or_else(|| {
                instance
                    .clone()
                    .as_interface::<dyn MemoryRead>()
                    .map(|read| read.read_location())
            });
        location == Some(address)
            && instance
                .operands()
                .iter()
                .filter(|&&v| v == address)
                .count()
                == 1
    })
}
