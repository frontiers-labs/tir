//! Transformation passes over generic TIR interfaces.

pub mod affine;
pub mod dce;
pub mod destructure;
pub mod erase_layout;
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

pub use affine::AffineSchedulePass;
pub use dce::DeadCodeEliminationPass;
pub use destructure::DestructurePass;
pub use erase_layout::EraseLayoutPass;
pub use inline::{InlineBudget, InlinePass};
pub use instcombine::InstCombineNodesPass;
pub use lower_intrinsics::LowerIntrinsicsPass;
pub use lower_ptr_disjoint::LowerPtrDisjointPass;
pub use materialize_symbol_addresses::MaterializeSymbolAddressesPass;
pub use promote_nodes::PromoteNodesPass;
pub use resolve_fp::ResolveFpPass;
pub use restructure::RestructureNodesPass;
pub use symbol_uniqueness::CheckUniqueSymbolsPass;
pub use verify_deps::VerifyDepsPass;
