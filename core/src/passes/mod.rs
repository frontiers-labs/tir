//! Transformation passes over generic TIR interfaces.

pub mod affine;
pub mod dce;
pub mod destructure;
pub mod inline;
pub mod instcombine;
pub mod lower_memory_intrinsics;
pub mod lower_ptr_disjoint;
pub mod materialize_symbol_addresses;
pub mod promote_nodes;
pub mod restructure;
pub mod symbol_uniqueness;
pub mod verify_deps;

pub use affine::{AffineSchedulePass, strip_mine};
pub use dce::DeadCodeEliminationPass;
pub use destructure::{
    CfgEdges, DestructurePass, Destructured, Edges, GateBlocks, LoopBlocks, Test, destructure,
};
pub use inline::{InlineBudget, InlinePass};
pub use instcombine::InstCombineNodesPass;
pub use lower_memory_intrinsics::LowerMemoryIntrinsicsPass;
pub use lower_ptr_disjoint::LowerPtrDisjointPass;
pub use materialize_symbol_addresses::MaterializeSymbolAddressesPass;
pub use promote_nodes::PromoteNodesPass;
pub use restructure::RestructureNodesPass;
pub use symbol_uniqueness::CheckUniqueSymbolsPass;
pub use verify_deps::{VerifyDepsPass, verify_deps};

use crate::{ConstantLike, Context, OpHandle, OpId, Pure, RegionId};

/// Every region under `root`, each one ahead of the regions nested inside it.
pub(crate) fn regions_under(context: &Context, root: OpId) -> Vec<RegionId> {
    let mut found = Vec::new();
    let mut pending = context.get_op(root).regions().to_vec();
    while let Some(region) = pending.pop() {
        found.push(region);
        for op in context.get_region(region).op_ids() {
            pending.extend(context.get_op(op).regions().iter().copied());
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
