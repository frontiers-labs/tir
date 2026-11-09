//! The counted loop a frontend raises while its body is still a block list.
//!
//! `restructure-nodes` turns it into an `scf.ordered_for`. Until then it only pins the
//! shape: the counter enters as port 0 and every other port takes an init, and
//! the body's `scf.yield` names the next value of each port past the counter.

use crate as tir;
use crate::binding;
use crate::parse::common::Cursor;
use crate::{Context, CountedLoop, Error, ExitScope, ExitScopeKind, ValueId};

tir::helpers::op_def! {
    file: "defs/converted/dialects_scf_ordered.cdsl",
    hosts: {
        ordered_for_verifier => crate::scf::ordered_for_verify_impl,
        ordered_for_iface_ExitScope => (ExitScope, &crate::scf::__TIR_IFACE_OrderedForOp_ExitScope),
        ordered_for_iface_CountedLoop => (CountedLoop, &crate::scf::__TIR_IFACE_OrderedForOp_CountedLoop),
        ordered_for_printer => crate::scf::ordered_for_custom_print,
        ordered_for_parser => |parser, context, _definition| crate::scf::ordered_for_custom_parse(parser, context),
    },
}

pub(crate) fn ordered_for_exitscope_exit_scope(__operation: &tir::OpHandle) -> ExitScopeKind {
    ExitScopeKind::Loop
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_OrderedForOp_ExitScope: crate::ExitScopeCallbacks =
    crate::ExitScopeCallbacks {
        exit_scope: crate::scf::ordered_for_exitscope_exit_scope,
        label: None,
    };

pub(crate) fn ordered_for_countedloop_lower_bound(operation: &tir::OpHandle) -> ValueId {
    operation.value_operands()[0]
}

pub(crate) fn ordered_for_countedloop_upper_bound(operation: &tir::OpHandle) -> ValueId {
    let operands = operation.value_operands();
    operands[operands.len() - 2]
}

pub(crate) fn ordered_for_countedloop_step(operation: &tir::OpHandle) -> ValueId {
    let operands = operation.value_operands();
    operands[operands.len() - 1]
}

pub(crate) fn ordered_for_countedloop_induction(__operation: &tir::OpHandle) -> Option<usize> {
    Some(0)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_OrderedForOp_CountedLoop: crate::CountedLoopCallbacks =
    crate::CountedLoopCallbacks {
        lower_bound: crate::scf::ordered_for_countedloop_lower_bound,
        upper_bound: crate::scf::ordered_for_countedloop_upper_bound,
        step: crate::scf::ordered_for_countedloop_step,
        induction: Some(crate::scf::ordered_for_countedloop_induction),
    };

/// What the ports past the counter take on the first iteration: the
/// declared inits, then the states threading has carried in.
pub fn ordered_for_inits(operation: &tir::OpHandle) -> Vec<ValueId> {
    let operands = operation.operands();
    operands[1..operands.len() - 2].to_vec()
}

pub(crate) fn ordered_for_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    let body = context.get_region(operation.regions()[0]);
    let ports: Vec<ValueId> = body.ports().iter().map(tir::Value::id).collect();
    let inits = crate::scf::ordered_for_inits(operation);
    tir::region_format::print_result_prefix(fmt, operation)?;
    fmt.write(format!(
        "scf.ordered_for %{} = %{} to %{} step %{}",
        ports[0].number(),
        crate::scf::ordered_for_countedloop_lower_bound(operation).number(),
        crate::scf::ordered_for_countedloop_upper_bound(operation).number(),
        ordered_for_countedloop_step(operation).number()
    ))?;
    binding::print_port_bindings(fmt, &ports[1..], &inits)?;
    tir::region_format::print_op_region(fmt, &context, operation, 0)
}

pub(crate) fn ordered_for_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    let counter_name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
        .to_string();
    binding::expect(parser, "=")?;
    let lb = binding::value(parser, context)?;
    binding::expect(parser, "to")?;
    let ub = binding::value(parser, context)?;
    binding::expect(parser, "step")?;
    let step = binding::value(parser, context)?;
    let bound = binding::parse_port_bindings(parser, context)?;

    let counter_type = context.get_value(lb).ty();
    let counter = context.create_value(counter_type, None);
    parser.define_value(&counter_name, counter.id());
    let mut ports = vec![counter];
    ports.extend(bound.ports.iter().cloned());
    let body = parser.parse_region_with_entry_args(context, ports)?.id();

    let mut result_types = vec![counter_type];
    result_types.extend(bound.ports.iter().map(tir::Value::ty));
    Ok(r#ordered_for(
        context,
        lb,
        bound.inits,
        ub,
        step,
        result_types,
        Some(body),
        Default::default(),
        tir::InsertionPoint::Root,
    ))
}

pub(crate) fn ordered_for_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    verify_body(context, operation)
}

fn verify_body(context: &Context, op: &crate::OpHandle) -> Result<(), Error> {
    let body = context.get_region(op.regions()[0]);
    let ports = body.ports();
    let inits = crate::scf::ordered_for_inits(op);
    let fail = |message: String| Err(Error::VerificationError(message));
    if ports.len() != inits.len() + 1 {
        return fail(format!(
            "scf.ordered_for takes {} initial values but its body carries {} ports past the counter",
            inits.len(),
            ports.len().saturating_sub(1)
        ));
    }
    for (index, (port, &init)) in ports[1..].iter().zip(&inits).enumerate() {
        if port.ty() != context.get_value(init).ty() {
            return fail(format!(
                "scf.ordered_for port {} and its init differ in type",
                index + 1
            ));
        }
    }
    let results = op.value_results();
    if results.len() != ports.len() {
        return fail(format!(
            "scf.ordered_for carries {} ports but has {} results",
            ports.len(),
            results.len()
        ));
    }
    let Some(entry) = body.block_ids().first().map(|&id| context.get_block(id)) else {
        return fail("scf.ordered_for has an empty body".into());
    };
    let Some(latch) = entry.op_ids().last().map(|&id| context.get_op(id)) else {
        return fail("scf.ordered_for body has no terminator".into());
    };
    if !latch.is_operation(&crate::scf::YIELD) {
        return fail("an scf.ordered_for body must end in scf.yield".into());
    }
    let yielded = latch.value_operands();
    if yielded.len() + 1 != ports.len() {
        return fail(format!(
            "scf.ordered_for carries {} values past its counter but its body yields {}",
            ports.len() - 1,
            yielded.len()
        ));
    }
    for (index, (port, &value)) in ports[1..].iter().zip(&yielded).enumerate() {
        if port.ty() != context.get_value(value).ty() {
            return fail(format!(
                "scf.ordered_for port {} and the value its body yields differ in type",
                index + 1
            ));
        }
    }
    Ok(())
}
