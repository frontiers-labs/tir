//! What an operation does to the memory it names, and where it names it.

use crate::func::CallOp;
use crate::ptr::MemcpyOp;
use crate::state::{JoinOp, SplitOp};
use crate::{MemoryRead, MemoryState, MemoryWrite, OpHandle, ValueId};

/// What one operation does to the memory it names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Observes a memory and leaves it as it found it.
    Read,
    /// Leaves a memory at an address that the reads after it see.
    Change,
}

/// What `op` does to memory, or `None` where it names none. Both memory
/// interfaces are asked before either answers: an operation declaring the two
/// writes the extent it reads, and is no observer. An operation naming a state
/// without declaring what it does to it changes it — a call, a copy, an export.
pub fn effect_of(op: &OpHandle) -> Option<Effect> {
    if op.has_interface::<dyn MemoryWrite>() || op.is::<MemcpyOp>() || op.is::<CallOp>() {
        return Some(Effect::Change);
    }
    if op.has_interface::<dyn MemoryRead>() {
        return Some(Effect::Read);
    }
    // A merge and a split only name the chains an effect crosses, and a loop or
    // a gate only carries them: the effect itself is inside.
    if op.is::<JoinOp>() || op.is::<SplitOp>() || !op.regions().is_empty() {
        return None;
    }
    (!op.state_operands().is_empty()).then_some(Effect::Change)
}

/// One declared access: where it lands, the value it names there, the state it
/// observes, and whether it puts the value in memory or takes it out.
pub struct Access {
    pub location: ValueId,
    pub value: ValueId,
    pub state: Option<ValueId>,
    pub write: bool,
}

/// The access `op` declares, read off whichever memory interface it carries.
pub fn access_of(op: &OpHandle) -> Option<Access> {
    if let Some(write) = op.clone().as_interface::<dyn MemoryWrite>() {
        return Some(Access {
            location: write.write_location(),
            value: write.written_value(),
            state: observed_state(op),
            write: true,
        });
    }
    let read = op.clone().as_interface::<dyn MemoryRead>()?;
    Some(Access {
        location: read.read_location(),
        value: read.read_value(),
        state: observed_state(op),
        write: false,
    })
}

/// The one state `op` observes, if it has been put on a chain.
pub fn observed_state(op: &OpHandle) -> Option<ValueId> {
    op.clone()
        .as_interface::<dyn MemoryState>()?
        .observed()
        .first()
        .copied()
}

/// The one state `op` leaves behind, if it has been put on a chain.
pub fn produced_state(op: &OpHandle) -> Option<ValueId> {
    op.clone()
        .as_interface::<dyn MemoryState>()?
        .produced()
        .first()
        .copied()
}
