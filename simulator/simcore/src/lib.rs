//! `tir-sim`: the simulation core. See `docs/design/simulator.md` for the
//! architecture overview.
//!
//! - [`ProgramImage`]/[`MachineBlock`] — assembly lowered to addressed blocks.
//! - [`Executor`] — the functional (architectural) interpreter and trace
//!   recorder.
//! - [`scoreboard`] — the shared cycle-assignment engine used by both
//!   `isasim --timing` (trace replay) and `tir sched` (static analysis).
//! - [`timing`] — trace-replay adapter over the scoreboard.
//! - [`predictor`] — swappable branch-direction policies.

pub mod error;
mod executor;
pub mod predictor;
mod program;
pub mod scoreboard;
pub mod timing;

pub use executor::*;
pub use program::*;
