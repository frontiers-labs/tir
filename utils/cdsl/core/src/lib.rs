//! Pure, formally verified CDSL v3 core algorithms (kernel type/erasure core + TEGG v15 core).
//!
//! Extracted to Lean 4 via Hax (`cargo xtask formal-test -p tir-cdsl-core`) and verified
//! against the standalone `CdslV3Spec` specifications.

// Explicit matches and `Vec::new() + push` keep Hax extraction to Lean 4 clean and total.
#![allow(
    clippy::collapsible_match,
    clippy::manual_map,
    clippy::vec_init_then_push
)]

pub mod kernel_core;
pub mod tegg_core;
