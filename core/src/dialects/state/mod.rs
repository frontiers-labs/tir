use crate::{Context, Error, Operation, builtin::StateResource, dialect, operation};

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
        scheme: "() -> ()",
        format: "custom",
        interfaces: [crate::interp::Interp],
        state: "out",
    }
}

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
operation! {
    JoinOp {
        name: "join",
        dialect: "state",
        scheme: "(state<_>*) -> ()",
        verifier: "true",
        operands: O { states },
        interfaces: [crate::interp::Interp],
        state: "out",
    }
}

impl JoinOp {
    /// The merged memory.
    pub fn result(&self) -> tir::ValueId {
        self.0.results()[0]
    }
}

impl tir::Verifiable for JoinOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        if self.0.operands().is_empty() {
            return Err(Error::VerificationError(
                "state.join merges at least one state".to_string(),
            ));
        }
        expect_states(&self.0, 1)?;
        let result_ty = context.get_value(self.result()).ty();
        if context.state_resource(result_ty) != Some(StateResource::Memory)
            || self
                .0
                .operands()
                .iter()
                .any(|operand| context.get_value(*operand).ty() != result_ty)
        {
            return Err(Error::VerificationError(
                "state.join only merges memory states of one type".into(),
            ));
        }
        Ok(())
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
        scheme: "() -> state<_>*",
        verifier: "true",
        results: R { states },
        interfaces: [crate::interp::Interp],
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

impl tir::Verifiable for SplitOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        if self.0.results().is_empty() {
            return Err(Error::VerificationError(
                "state.split names at least one chain".to_string(),
            ));
        }
        expect_states(&self.0, self.0.results().len())?;
        let observed_ty = context.get_value(self.observed()).ty();
        if context.state_resource(observed_ty) != Some(StateResource::Memory)
            || self
                .0
                .results()
                .iter()
                .any(|result| context.get_value(*result).ty() != observed_ty)
        {
            return Err(Error::VerificationError(
                "state.split only partitions memory states of one type".into(),
            ));
        }
        Ok(())
    }
}

fn expect_states(op: &tir::OpHandle, results: usize) -> Result<(), Error> {
    let (dialect, name) = (op.dialect(), op.name());
    if op.state_results().len() != results || !op.value_results().is_empty() {
        return Err(Error::VerificationError(format!(
            "{dialect}.{name} produces {results} states"
        )));
    }
    Ok(())
}
