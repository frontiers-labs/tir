pub mod defuse;
pub mod effects;
pub mod exits;
mod manager;
pub(crate) mod memory;
pub mod regions;
pub mod value_info;
pub use defuse::{DefUse, OpRegs, PhysReg, execution_regs, op_regs};
pub use effects::{Access, Effect, access_of, effect_of};
pub use manager::*;

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/analyses.rs"));
}
