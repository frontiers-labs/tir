//! Transformation passes over generic TIR interfaces.

pub mod affine;
pub mod dce;
pub mod destructure;
pub mod inline;
pub mod instcombine;
pub mod lower_intrinsics;
pub mod lower_ptr_disjoint;
pub mod materialize_symbol_addresses;
pub mod promote_nodes;
pub mod resolve_fp;
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
pub use lower_intrinsics::LowerIntrinsicsPass;
pub use lower_ptr_disjoint::LowerPtrDisjointPass;
pub use materialize_symbol_addresses::MaterializeSymbolAddressesPass;
pub use promote_nodes::PromoteNodesPass;
pub use resolve_fp::ResolveFpPass;
pub use restructure::RestructureNodesPass;
pub use symbol_uniqueness::CheckUniqueSymbolsPass;
pub use verify_deps::{VerifyDepsPass, verify_deps};

use std::collections::HashSet;

use crate::{ConstantLike, Context, OpHandle, OpId, Pure, RegionId, ValueId};

/// Every region under `root`, each one ahead of the regions nested inside it.
pub(crate) fn regions_under(context: &Context, root: OpId) -> Vec<RegionId> {
    context
        .get_op(root)
        .regions()
        .iter()
        .flat_map(|&region| context.nested_regions(region))
        .collect()
}

/// The operations the results of `roots` demand: an operation is demanded
/// through an operand of a demanded one, or through the results of a region of
/// one. An operation in a region nobody demands is demanded by nothing, however
/// its own region reads it.
pub(crate) fn demanded_ops(context: &Context, roots: &[RegionId]) -> HashSet<OpId> {
    let defining = |values: Vec<ValueId>| {
        values
            .into_iter()
            .filter_map(|value| context.get_value(value).defining_op())
            .collect::<Vec<_>>()
    };
    let mut worklist: Vec<OpId> = roots
        .iter()
        .flat_map(|&region| defining(context.get_region(region).results()))
        .collect();
    let mut demanded = HashSet::new();
    while let Some(op) = worklist.pop() {
        if !demanded.insert(op) {
            continue;
        }
        let instance = context.get_op(op);
        worklist.extend(defining(instance.operands().to_vec()));
        for region in instance.regions() {
            worklist.extend(defining(context.get_region(region).results()));
        }
    }
    demanded
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
