use crate::{Terminator, dialect};

use crate as tir;

pub mod nodes;
pub mod ordered;
pub use nodes::*;
pub use ordered::*;

pub mod ops {
    pub use super::nodes::*;
    pub use super::ordered::*;
    pub use super::{YIELD, r#yield};
}

dialect! {
    ScfDialect {
        name: "scf",
        operations: [
            LOOP,
            SWITCH,
            FOR,
            ORDERED_FOR,
            YIELD,
        ],
        types: [],
    }
}

// The back edge of an `scf.ordered_for`: it names what the next iteration
// carries. An unordered body names its results outright and has no
// terminator, so nothing else needs this.
tir::helpers::op_def! {
    file: "defs/converted/dialects_scf_mod.cdsl",
    hosts: {
        yield_iface_Terminator => (Terminator, &crate::TerminatorCallbacks::DEFAULT),
    },
}
