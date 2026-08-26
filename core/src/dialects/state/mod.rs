use crate::parse::common::{Cursor, Span};
use crate::{Context, Error, IRFormatter, Operation, dialect, operation};

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

// The memory every input names, merged. Reads leave memory as they found it, so
// a fork of reads off one write is joined back into the state the write left; a
// write, a call or an export after them takes the join, which is the edge that
// orders it after every read of the fork.
operation! {
    JoinOp {
        name: "join",
        dialect: "state",
        format: "custom",
        operands: O {
            states: "*crate::builtin::StateType",
        },
        results: R {
            result: "crate::builtin::StateType",
        },
        interfaces: [crate::interp::Interp],
    }
}

impl JoinOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        fmt.write(format!("%{} = state.join", self.result().number()))?;
        for (index, state) in self.operands().iter().enumerate() {
            fmt.write(if index == 0 { " " } else { ", " })?;
            fmt.write(format!("%{}", state.number()))?;
        }
        fmt.write(" : ")?;
        context.print_type(context.get_value(self.result()).ty(), fmt)?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut crate::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let mut states = vec![];
        while let Some(reference) = parser.parse_value_ref() {
            states.push(parser.resolve_value(context, reference));
            if !parser.parse_token(",") {
                break;
            }
        }
        if !parser.parse_token(":") {
            return Err((parser.span(), Error::ExpectedToken(":")));
        }
        let result_type = parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        Ok(Box::new(
            JoinOpBuilder::new(context)
                .states(states)
                .result_type(result_type)
                .build(),
        ))
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
        format: "custom",
        operands: O {
            state: "crate::builtin::StateType",
        },
        results: R {
            states: "*crate::builtin::StateType",
        },
        interfaces: [crate::interp::Interp],
    }
}

impl SplitOp {
    /// The one memory the chains crossing this split carry on from.
    pub fn observed(&self) -> tir::ValueId {
        self.operands()[0]
    }

    /// One state per chain crossing the split.
    pub fn states(&self) -> Vec<tir::ValueId> {
        self.0.results().to_vec()
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        for (index, state) in self.states().iter().enumerate() {
            fmt.write(if index == 0 { "" } else { ", " })?;
            fmt.write(format!("%{}", state.number()))?;
        }
        fmt.write(format!(" = state.split %{} : ", self.observed().number()))?;
        for (index, state) in self.states().iter().enumerate() {
            fmt.write(if index == 0 { "" } else { ", " })?;
            context.print_type(context.get_value(*state).ty(), fmt)?;
        }
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut crate::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let reference = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
        let state = parser.resolve_value(context, reference);
        if !parser.parse_token(":") {
            return Err((parser.span(), Error::ExpectedToken(":")));
        }
        let mut types = vec![];
        loop {
            types.push(
                parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?,
            );
            if !parser.parse_token(",") {
                break;
            }
        }
        Ok(Box::new(
            SplitOpBuilder::new(context)
                .state(state)
                .result_types(types)
                .build(),
        ))
    }
}
