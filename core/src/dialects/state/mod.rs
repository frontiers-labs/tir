use crate::{Context, Error, MemoryState, dialect, operation};

use crate as tir;

pub mod ops {
    pub use super::{EntryStateOp, JoinOp, SplitOp, entry_state, join, split};
}

dialect! {
    StateDialect {
        name: "state",
        operations: [EntryStateOp, JoinOp, SplitOp],
        types: [],
    }
}

// The memory a function is entered with, one op per chain. Spelled
// `state(%s) = state.entry_state`: it carries a state and nothing else.
operation! {
    EntryStateOp {
        name: "entry_state",
        dialect: "state",
        interfaces: [MemoryState, crate::interp::Interp],
        state: "out",
    }
}

impl EntryStateOp {
    /// The chain this op opens.
    pub fn result(&self) -> tir::ValueId {
        self.0.results()[0]
    }
}

impl MemoryState for EntryStateOp {
    fn observed(&self) -> Vec<tir::ValueId> {
        Vec::new()
    }

    fn produced(&self) -> Vec<tir::ValueId> {
        self.0.results().to_vec()
    }

    fn changes_memory(&self) -> bool {
        false
    }
}

// The memory every input names, merged. Reads leave memory as they found it, so
// a fork of reads off one write is joined back into the state the write left; a
// write, a call or an export after them takes the join, which is the edge that
// orders it after every read of the fork.
operation! {
    JoinOp {
        name: "join",
        dialect: "state",
        verifier: "true",
        operands: O {
            states: "*crate::builtin::StateType",
        },
        interfaces: [MemoryState, crate::interp::Interp],
        state: "out",
    }
}

impl JoinOp {
    /// The merged memory.
    pub fn result(&self) -> tir::ValueId {
        self.0.results()[0]
    }
}

impl MemoryState for JoinOp {
    fn observed(&self) -> Vec<tir::ValueId> {
        self.0.operands().to_vec()
    }

    fn produced(&self) -> Vec<tir::ValueId> {
        self.0.results().to_vec()
    }

    fn changes_memory(&self) -> bool {
        false
    }
}

impl tir::Verifiable for JoinOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        if self.0.operands().is_empty() {
            return Err(Error::VerificationError(
                "state.join merges at least one state".to_string(),
            ));
        }
        expect_states(&self.0, 1)
    }
}

// One memory named once per chain that crosses it. A call touches every object
// the outside can reach, so the chains it may clobber are joined into the state it
// observes and split back out of the state it leaves: each chain carries on from a
// name of its own, ordered after the call.
operation! {
    SplitOp {
        name: "split",
        dialect: "state",
        verifier: "true",
        results: R {
            states: "*crate::builtin::StateType",
        },
        interfaces: [MemoryState, crate::interp::Interp],
        state: "in",
    }
}

impl SplitOp {
    /// The one memory the chains crossing this split carry on from.
    pub fn observed(&self) -> tir::ValueId {
        self.0.operands()[0]
    }

    /// One state per chain crossing the split.
    pub fn states(&self) -> Vec<tir::ValueId> {
        self.0.results().to_vec()
    }
}

impl MemoryState for SplitOp {
    fn observed(&self) -> Vec<tir::ValueId> {
        self.0.operands().to_vec()
    }

    fn produced(&self) -> Vec<tir::ValueId> {
        self.0.results().to_vec()
    }

    fn changes_memory(&self) -> bool {
        false
    }
}

impl tir::Verifiable for SplitOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        if self.0.results().is_empty() {
            return Err(Error::VerificationError(
                "state.split names at least one chain".to_string(),
            ));
        }
        expect_states(&self.0, self.0.results().len())
    }
}

/// These ops leave `results` states behind and nothing else.
fn expect_states(op: &tir::OpHandle, results: usize) -> Result<(), Error> {
    let (dialect, name) = (op.dialect(), op.name());
    if op.state_results().len() != results || !op.value_results().is_empty() {
        return Err(Error::VerificationError(format!(
            "{dialect}.{name} produces {results} states"
        )));
    }
    Ok(())
}
