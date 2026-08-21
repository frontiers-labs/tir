use crate::{dialect, operation};

use crate as tir;

pub mod ops {
    pub use super::{EntryStateOp, entry_state};
}

dialect! {
    StateDialect {
        name: "state",
        operations: [EntryStateOp],
        types: [],
    }
}

operation! {
    EntryStateOp {
        name: "entry_state",
        dialect: "state",
        results: R {
            result: "crate::builtin::StateType",
        },
        interfaces: [crate::interp::Interp],
    }
}
