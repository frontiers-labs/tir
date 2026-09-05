use crate::parse::common::Span;
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

// The memory a function is entered with, one op per chain. Spelled
// `| %s = state.entry_state`: it carries dependencies and nothing else.
operation! {
    EntryStateOp {
        name: "entry_state",
        dialect: "state",
        format: "custom",
        verifier: "true",
        interfaces: [crate::interp::Interp],
    }
}

impl EntryStateOp {
    /// The chain this op opens.
    pub fn result(&self) -> tir::ValueId {
        self.0.dep_results()[0]
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        tir::dependency::print_result_prefix(fmt, &self.0)?;
        fmt.write("state.entry_state\n")
    }

    fn custom_parse(
        _parser: &mut crate::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        Ok(Box::new(EntryStateOpBuilder::new(context).build()))
    }
}

impl tir::Verifiable for EntryStateOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        expect_deps(&self.0, 0, 1)
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
        verifier: "true",
        interfaces: [crate::interp::Interp],
    }
}

impl JoinOp {
    /// The merged memory.
    pub fn result(&self) -> tir::ValueId {
        self.0.dep_results()[0]
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        tir::dependency::print_result_prefix(fmt, &self.0)?;
        fmt.write("state.join")?;
        tir::dependency::print_dep_operands(fmt, &self.0)?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut crate::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let mut builder = JoinOpBuilder::new(context);
        for state in tir::dependency::parse_dep_operands(parser, context)? {
            builder = builder.dep_operand(state);
        }
        Ok(Box::new(builder.build()))
    }
}

impl tir::Verifiable for JoinOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        if self.0.dep_operands().is_empty() {
            return Err(Error::VerificationError(
                "state.join merges at least one dependency".to_string(),
            ));
        }
        expect_deps(&self.0, self.0.dep_operands().len(), 1)
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
        verifier: "true",
        interfaces: [crate::interp::Interp],
    }
}

impl SplitOp {
    /// The one memory the chains crossing this split carry on from.
    pub fn observed(&self) -> tir::ValueId {
        self.0.dep_operands()[0]
    }

    /// One state per chain crossing the split.
    pub fn states(&self) -> Vec<tir::ValueId> {
        self.0.dep_results().to_vec()
    }

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        tir::dependency::print_result_prefix(fmt, &self.0)?;
        fmt.write("state.split")?;
        tir::dependency::print_dep_operands(fmt, &self.0)?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut crate::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let mut builder = SplitOpBuilder::new(context);
        for state in tir::dependency::parse_dep_operands(parser, context)? {
            builder = builder.dep_operand(state);
        }
        Ok(Box::new(builder.build()))
    }
}

impl tir::Verifiable for SplitOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        if self.0.dep_results().is_empty() {
            return Err(Error::VerificationError(
                "state.split names at least one chain".to_string(),
            ));
        }
        expect_deps(&self.0, 1, self.0.dep_results().len())
    }
}

/// These ops carry nothing but dependencies, at the arity given.
fn expect_deps(op: &tir::OpHandle, operands: usize, results: usize) -> Result<(), Error> {
    let (dialect, name) = (op.dialect(), op.name());
    if !op.value_operands().is_empty() || !op.value_results().is_empty() {
        return Err(Error::VerificationError(format!(
            "{dialect}.{name} carries only dependencies"
        )));
    }
    if op.dep_operands().len() != operands || op.dep_results().len() != results {
        return Err(Error::VerificationError(format!(
            "{dialect}.{name} takes {operands} dependencies and produces {results}"
        )));
    }
    Ok(())
}
