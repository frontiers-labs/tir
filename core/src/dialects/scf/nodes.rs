//! Structured control flow: a loop and a switch whose bindings are declared,
//! and a counted loop that pins a loop's shape.

use crate as tir;
use crate::attributes::Predicate;
use crate::binding::{self, PortBindings};
use crate::builtin::IntegerType;
use crate::parse::common::Cursor;
use crate::{Context, Error, ExitScope, ExitScopeKind, ValueId};

// A θ: every carried value enters as a port, and the body names the predicate,
// the values the next iteration carries, then the values the loop produces
// once the predicate is false. Only the cone the predicate selects runs.
tir::helpers::op_def! {
    file: "defs/converted/dialects_scf_nodes.cdsl",
    hosts: {
        loop_iface_ExitScope => (ExitScope, &crate::scf::__TIR_IFACE_LoopOp_ExitScope),
        switch_iface_ExitScope => (ExitScope, &crate::scf::__TIR_IFACE_SwitchOp_ExitScope),
        for_iface_ExitScope => (ExitScope, &crate::scf::__TIR_IFACE_ForOp_ExitScope),
        for_printer => crate::scf::for_custom_print,
        for_parser => |parser, context, _definition| crate::scf::for_custom_parse(parser, context),
    },
}

pub(crate) fn loop_exitscope_exit_scope(___operation: &tir::OpHandle) -> ExitScopeKind {
    ExitScopeKind::Loop
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_LoopOp_ExitScope: crate::ExitScopeCallbacks =
    crate::ExitScopeCallbacks {
        exit_scope: crate::scf::loop_exitscope_exit_scope,
        label: None,
    };

// A γ: arm `i` runs when the predicate equals case `i`, and the last arm, the
// default, takes every other value. Without a `cases` attribute the cases are
// the arm indices, so a predicate past the last arm selects the last arm.

pub(crate) fn switch_exitscope_exit_scope(___operation: &tir::OpHandle) -> ExitScopeKind {
    ExitScopeKind::Switch
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SwitchOp_ExitScope: crate::ExitScopeCallbacks =
    crate::ExitScopeCallbacks {
        exit_scope: crate::scf::switch_exitscope_exit_scope,
        label: None,
    };

// A counted loop: a θ whose port 0 is the counter, starting at `lb`, tested
// against `ub` with `cmpi slt` and advanced by `step` with `addi`, and whose
// exit values are its ports. The bounds count, so they are integers or
// indices.
//
// The text elides what the shape pins: the counter's comparison, its
// increment, and the exit values. `%i, %r = scf.for %c = %lb to %ub step %s
// (%a = %init) { .. -> %next }` names the counter's final value first.

pub(crate) fn for_exitscope_exit_scope(___operation: &tir::OpHandle) -> ExitScopeKind {
    ExitScopeKind::Loop
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_ForOp_ExitScope: crate::ExitScopeCallbacks =
    crate::ExitScopeCallbacks {
        exit_scope: crate::scf::for_exitscope_exit_scope,
        label: None,
    };

pub(crate) fn for_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    let body = context.get_region(for_theta_body(operation));
    let binding = for_theta_binding(operation);
    let ports: Vec<ValueId> = body.ports().iter().map(tir::Value::id).collect();
    let inits = operation.operands()[binding.operands.clone()].to_vec();
    let results = body.results();
    // Unverified IR may lack the shape the text relies on; print it whole.
    if ports.is_empty() || inits.is_empty() || results.len() < binding.continue_.end {
        return generic_print(fmt, &context, operation);
    }

    tir::region_format::print_result_prefix(fmt, operation)?;
    fmt.write(format!(
        "scf.for %{} = %{} to %{} step %{}",
        ports[0].number(),
        inits[0].number(),
        for_countedloop_upper_bound(operation).number(),
        for_countedloop_step(operation).number()
    ))?;
    binding::print_port_bindings(fmt, &ports[1..], &inits[1..])?;
    let shown = &results[binding.continue_.start + 1..binding.continue_.end];
    let hidden: Vec<tir::OpId> = [results[0], results[binding.continue_.start]]
        .iter()
        .filter(|&&value| !context.is_used(value) && !shown.contains(&value))
        .filter_map(|&value| context.get_value(value).defining_op())
        .collect();
    tir::region_format::print_nodes_region_with(fmt, &context, &body, &hidden, shown)
}

pub(crate) fn for_custom_parse(
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
    let mut ports = vec![counter.clone()];
    ports.extend(bound.ports.iter().cloned());
    let body = parser.parse_region_with_entry_args(context, ports)?.id();
    materialize_counted_shape(context, body, counter.id(), ub, step, &bound)
        .map_err(|error| (parser.span(), error))?;

    let mut result_types = vec![counter_type];
    result_types.extend(bound.ports.iter().map(tir::Value::ty));
    Ok(r#for(
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

fn generic_print(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    op: &crate::OpHandle,
) -> Result<(), std::fmt::Error> {
    tir::region_format::print_result_prefix(fmt, op)?;
    fmt.write("scf.for")?;
    tir::region_format::print_region(fmt, context, &context.get_region(for_theta_body(op)))
}

fn materialize_counted_shape(
    context: &Context,
    body: tir::RegionId,
    counter: ValueId,
    ub: ValueId,
    step: ValueId,
    bound: &PortBindings,
) -> Result<(), Error> {
    let region = context.get_region(body);
    let written = region.results();
    if written.len() != bound.ports.len() {
        return Err(Error::VerificationError(format!(
            "scf.for carries {} values but its body names {}",
            bound.ports.len(),
            written.len()
        )));
    }
    let existing = |wanted: &dyn Fn(&tir::OpHandle) -> bool| {
        region
            .op_ids()
            .into_iter()
            .map(|op| context.get_op(op))
            .find(|op| wanted(op))
            .map(|op| op.results()[0])
    };
    let compare = existing(&|op| {
        op.is_operation(&crate::builtin::CMPI)
            && op.attr("predicate")
                == Some(tir::attributes::AttributeValue::Predicate(Predicate::Slt))
            && op.operands().as_slice() == [counter, ub]
    })
    .unwrap_or_else(|| {
        let compare = crate::builtin::r#cmpi(
            context,
            counter,
            ub,
            Predicate::Slt,
            IntegerType::new(context, 1),
            Default::default(),
            tir::InsertionPoint::Region(body),
        );
        compare.results()[0]
    });
    let advance = existing(&|op| {
        op.is_operation(&crate::builtin::ADDI) && op.operands().as_slice() == [counter, step]
    })
    .unwrap_or_else(|| {
        let advance = crate::builtin::r#addi(
            context,
            counter,
            step,
            context.get_value(counter).ty(),
            Default::default(),
            tir::InsertionPoint::Region(body),
        );
        advance.results()[0]
    });

    let mut results = vec![compare, advance];
    results.extend(written);
    results.push(counter);
    results.extend(bound.ports.iter().map(tir::Value::id));
    context.set_region_results(body, results);
    Ok(())
}
