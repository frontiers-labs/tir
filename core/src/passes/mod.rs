//! Transformation passes over generic TIR interfaces.

pub mod affine;
pub(crate) mod common;
pub mod control_simplify;
pub mod dce;
pub mod destructure;
pub mod eqsat;
pub mod erase_layout;
pub mod inline;
pub mod instcombine;
pub mod lower_intrinsics;
pub mod lower_ptr_disjoint;
pub mod materialize_symbol_addresses;
pub mod memopt;
pub mod promote_nodes;
pub mod restructure;
pub mod symbol_uniqueness;
pub mod verify_deps;

pub use affine::AffineSchedulePass;
pub use control_simplify::ControlSimplifyPass;
pub use dce::DeadCodeEliminationPass;
pub use destructure::DestructurePass;
pub use eqsat::EqSatPass;
pub use erase_layout::EraseLayoutPass;
pub use inline::{InlineBudget, InlinePass};
pub use instcombine::InstCombineNodesPass;
pub use lower_intrinsics::LowerIntrinsicsPass;
pub use lower_ptr_disjoint::LowerPtrDisjointPass;
pub use materialize_symbol_addresses::MaterializeSymbolAddressesPass;
pub use memopt::MemoryOptPass;
pub use promote_nodes::PromoteNodesPass;
pub use restructure::RestructureNodesPass;
pub use symbol_uniqueness::CheckUniqueSymbolsPass;
pub use verify_deps::VerifyDepsPass;

/// Append the standard mid-end simplification sequence (`eqsat`, `memopt`,
/// `control-simplify`, `dce`) to `pm`.
pub fn add_simplify_pipeline(pm: &mut crate::PassManager) {
    pm.add_pass(EqSatPass::new())
        .add_pass(MemoryOptPass::new())
        .add_pass(ControlSimplifyPass::new())
        .add_pass(DeadCodeEliminationPass::new());
}
