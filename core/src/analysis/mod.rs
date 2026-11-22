pub mod cfg;
pub mod defuse;
pub mod effects;
pub mod exits;
pub mod regions;
pub use defuse::{DefUse, OpRegs, PhysReg, execution_regs, op_regs};
pub use effects::{Access, Effect, access_of, effect_of};

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/analyses.rs"));
}
