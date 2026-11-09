use crate::{Context, Error, builtin::StateResource, dialect};

use crate as tir;

pub mod ops {
    pub use super::{ENTRY_STATE, JOIN, SPLIT, entry_state, join, split};
}

dialect! {
    StateDialect {
        name: "state",
        operations: [ENTRY_STATE, JOIN, SPLIT],
        types: [],
    }
}

tir::helpers::op_def! {
    file: "defs/converted/dialects_state_mod.cdsl",
    hosts: {
        entry_state_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_EntryStateOp_Interp),
        entry_state_printer => crate::state::entry_state_custom_print,
        entry_state_parser => |parser, context, _definition| crate::state::entry_state_custom_parse(parser, context),
        join_verifier => crate::state::join_verify_impl,
        join_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_JoinOp_Interp),
        split_verifier => crate::state::split_verify_impl,
        split_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_SplitOp_Interp),
    },
}

/// The chain this op opens.
pub fn entry_state_result(operation: &tir::OpHandle) -> tir::ValueId {
    operation.results()[0]
}

pub(crate) fn entry_state_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    tir::region_format::print_result_prefix(fmt, operation)?;
    fmt.write("state.entry_state : ")?;
    operation.context.print_type(
        operation
            .context
            .get_value(crate::state::entry_state_result(operation))
            .ty(),
        fmt,
    )?;
    fmt.write("\n")
}

pub(crate) fn entry_state_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
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
    Ok(crate::state::r#entry_state(
        context,
        vec![ty],
        Default::default(),
        tir::InsertionPoint::Root,
    ))
}

// The memory every input names, merged. Reads leave memory as they found it, so
// a fork of reads off one write is joined back into the state the write left; a
// write, a call or an export after them takes the join, which is the edge that
// orders it after every read of the fork.

/// The merged memory.
pub fn join_result(operation: &tir::OpHandle) -> tir::ValueId {
    operation.results()[0]
}

pub(crate) fn join_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    if operation.operands().is_empty() {
        return Err(Error::VerificationError(
            "state.join merges at least one state".to_string(),
        ));
    }
    expect_states(operation, 1)?;
    let result_ty = context.get_value(crate::state::join_result(operation)).ty();
    if context.state_resource(result_ty) != Some(StateResource::Memory)
        || operation
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

// One memory named once per chain that crosses it. A call touches every object
// the outside can reach, so the chains it may clobber are joined into the state it
// observes and split back out of the state it leaves: each chain carries on from a
// name of its own, ordered after the call.

/// The one memory the chains crossing this split carry on from.
pub fn split_observed(operation: &tir::OpHandle) -> tir::ValueId {
    operation.operands()[0]
}

/// One state per chain crossing the split.
pub fn split_states(operation: &tir::OpHandle) -> Vec<tir::ValueId> {
    operation.results().to_vec()
}

pub(crate) fn split_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    if operation.results().is_empty() {
        return Err(Error::VerificationError(
            "state.split names at least one chain".to_string(),
        ));
    }
    expect_states(operation, operation.results().len())?;
    let observed_ty = context
        .get_value(crate::state::split_observed(operation))
        .ty();
    if context.state_resource(observed_ty) != Some(StateResource::Memory)
        || operation
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

fn expect_states(op: &tir::OpHandle, results: usize) -> Result<(), Error> {
    let (dialect, name) = (op.dialect(), op.name());
    if op.state_results().len() != results || !op.value_results().is_empty() {
        return Err(Error::VerificationError(format!(
            "{dialect}.{name} produces {results} states"
        )));
    }
    Ok(())
}
