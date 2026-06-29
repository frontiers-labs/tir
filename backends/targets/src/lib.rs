//! Target aggregator shared by `tir-mc` and `isasim`.
//!
//! Each backend crate registers its own [`TargetMachine`] with the link-time
//! registry in `tir-be-common` via `register_target!`. This crate exists to
//! pull those backends into the dependency graph so their registrations are
//! linked, and to re-export the registry's lookup helpers under stable names.
//! Adding a backend means depending on its crate here — nothing in the tools
//! changes.

// Force the backend crates to be linked so their `register_target!` entries are
// included in the final binary; the registry is otherwise the only user.
use tir_arm64 as _;
use tir_riscv as _;
use tir_x86_64 as _;

pub use tir_be_common::{select_target as select, supported_targets};
