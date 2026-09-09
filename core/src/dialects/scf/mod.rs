use crate::{Terminator, dialect, operation};

use crate as tir;
use crate::Any as AnyConstraint;

pub mod nodes;
pub mod ordered;
pub use nodes::{ForOp, ForOpBuilder, LoopOp, LoopOpBuilder, SwitchOp, SwitchOpBuilder};
pub use ordered::{OrderedForOp, OrderedForOpBuilder};

pub mod ops {
    pub use super::nodes::{r#for, r#loop, switch};
    pub use super::{YieldOp, r#yield};
}

dialect! {
    ScfDialect {
        name: "scf",
        operations: [
            LoopOp,
            SwitchOp,
            ForOp,
            OrderedForOp,
            YieldOp,
        ],
        types: [],
    }
}

// The back edge of an `scf.ordered_for`: it names what the next iteration
// carries. An unordered body names its results outright and has no
// terminator, so nothing else needs this.
operation! {
    YieldOp {
        name: "yield",
        dialect: "scf",
        operands: O {
            values: "*AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for YieldOp {}
