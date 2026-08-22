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
