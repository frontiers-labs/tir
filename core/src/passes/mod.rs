//! Transformation passes over generic TIR interfaces.

pub mod dce;
pub mod erase_state;
pub mod instcombine;
pub mod lower_memory_intrinsics;
pub mod materialize_symbol_addresses;
pub mod restructure;
pub mod sccp;
pub mod symbol_uniqueness;
pub mod thread_state;

pub use dce::DeadCodeEliminationPass;
pub use erase_state::EraseStatePass;
pub use instcombine::InstCombinePass;
pub use lower_memory_intrinsics::LowerMemoryIntrinsicsPass;
pub use materialize_symbol_addresses::MaterializeSymbolAddressesPass;
pub use restructure::RestructurePass;
pub use sccp::SccpPass;
pub use symbol_uniqueness::CheckUniqueSymbolsPass;
pub use thread_state::ThreadStatePass;

use crate::{Context, OpId, RegionId};

/// Every region under `root`, each one ahead of the regions nested inside it.
pub(crate) fn regions_under(context: &Context, root: OpId) -> Vec<RegionId> {
    let mut found = Vec::new();
    let mut pending = context.get_op(root).regions().to_vec();
    while let Some(region) = pending.pop() {
        found.push(region);
        for block in context.get_region(region).iter(context.clone()) {
            for op in block.op_ids() {
                pending.extend(context.get_op(op).regions().iter().copied());
            }
        }
    }
    found
}
