pub mod affine;
pub mod alias_facts;
pub mod defuse;
mod dominance;
pub mod escape_facts;
mod manager;
pub mod scopes;
pub mod slots;
pub mod solver;

pub use affine::AffineView;
pub use alias_facts::{AliasFacts, AliasResult, Base, PointerFact};
pub use defuse::{DefUse, OpRegs, PhysReg, execution_regs, op_regs};
pub use dominance::*;
pub use escape_facts::{Escape, EscapeFacts};
pub use manager::*;
