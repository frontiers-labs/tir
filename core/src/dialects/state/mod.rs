use crate::{Context, Error, Operation, dialect};

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

__cdsl_state_EntryStateOp!();

impl EntryStateOp {
    /// The chain this op opens.
    pub fn result(&self) -> tir::ValueId {
        self.0.results()[0]
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        tir::region_format::print_result_prefix(fmt, &self.0)?;
        fmt.write("state.entry_state : ")?;
        self.0
            .context
            .print_type(self.0.context.get_value(self.result()).ty(), fmt)?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        use tir::parse::common::Cursor;

        if !parser.parse_token(":") {
            return Err((parser.span(), Error::ExpectedToken(":")));
        }
        let ty = parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        if context.state_resource(ty).is_none() {
            return Err((parser.span(), Error::ExpectedType));
        }
        Ok(Box::new(
            EntryStateOpBuilder::new(context).state_result(ty).build(),
        ))
    }
}

// The memory every input names, merged. Reads leave memory as they found it, so
// a fork of reads off one write is joined back into the state the write left; a
// write, a call or an export after them takes the join, which is the edge that
// orders it after every read of the fork.
__cdsl_state_JoinOp!();

impl JoinOp {
    /// The merged memory.
    pub fn result(&self) -> tir::ValueId {
        self.0.results()[0]
    }
}

// One memory named once per chain that crosses it. A call touches every object
// the outside can reach, so the chains it may clobber are joined into the state it
// observes and split back out of the state it leaves: each chain carries on from a
// name of its own, ordered after the call.
__cdsl_state_SplitOp!();

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
