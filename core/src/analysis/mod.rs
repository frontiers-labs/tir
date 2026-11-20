pub mod defuse;
pub mod effects;
pub mod exits;
mod manager;
pub mod regions;
pub use defuse::{DefUse, OpRegs, PhysReg, execution_regs, op_regs};
pub use effects::{Access, Effect, access_of, effect_of};
pub use manager::*;
