//! Transformation passes over generic TIR interfaces.

pub mod dce;
pub mod dse;
pub mod erase_state;
pub mod instcombine;
pub mod lower_memory_intrinsics;
pub mod materialize_symbol_addresses;
pub mod restructure;
pub mod sccp;
pub mod shuffle_state;
pub mod symbol_uniqueness;
pub mod thread_state;

pub use dce::DeadCodeEliminationPass;
pub use dse::DeadStoreEliminationPass;
pub use erase_state::EraseStatePass;
pub use instcombine::InstCombinePass;
pub use lower_memory_intrinsics::LowerMemoryIntrinsicsPass;
pub use materialize_symbol_addresses::MaterializeSymbolAddressesPass;
pub use restructure::RestructurePass;
pub use sccp::SccpPass;
pub use shuffle_state::ShuffleStatePass;
pub use symbol_uniqueness::CheckUniqueSymbolsPass;
pub use thread_state::ThreadStatePass;

use crate::{
    ConstantLike, Context, OpHandle, OpId, OperationRef, PassError, Pure, RegionId, Rewriter,
    TypeId, ValueId,
};

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

/// A value op the transforms may reason about as an expression: one that
/// declares purity, a literal, or one whose semantics the vocabulary spells.
pub(crate) fn is_pure_value(instance: &OpHandle) -> bool {
    instance.clone().as_interface::<dyn Pure>().is_some()
        || instance
            .clone()
            .as_interface::<dyn ConstantLike>()
            .is_some()
        || instance
            .clone()
            .as_dyn_op()
            .semantic_expr(&mut crate::sem::SemGraph::new())
            .is_some()
}

/// Build the literal `value` of type `ty` where `target` sits, and hand back
/// what it defines. Which literal to build and where is a pass's own policy;
/// building one is this.
pub(crate) fn literal_before(
    context: &Context,
    rewriter: &mut Rewriter,
    value: i64,
    ty: TypeId,
    target: &OperationRef,
) -> Result<ValueId, PassError> {
    let op = crate::builtin::ops::constant(context, value, ty).build();
    rewriter.insert_op_before(target, &op)?;
    Ok(op.result())
}
