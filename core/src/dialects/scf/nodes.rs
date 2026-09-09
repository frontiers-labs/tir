//! Structured control flow: a loop and a switch whose bindings are declared,
//! and a counted loop that pins a loop's shape.

use crate as tir;
use crate::Any as AnyConstraint;
use crate::attributes::Predicate;
use crate::binding::{self, PortBindings};
use crate::builtin::{AddIOpBuilder, CmpIOpBuilder, IntegerType};
use crate::parse::common::Cursor;
use crate::{Context, Error, ExitScope, ExitScopeKind, Operation, Theta, ValueId, operation};

// A θ: every carried value enters as a port, and the body names the predicate,
// the values the next iteration carries, then the values the loop produces
// once the predicate is false. Only the cone the predicate selects runs.
operation! {
    LoopOp {
        name: "loop",
        dialect: "scf",
        operands: O {
            inits: "*AnyConstraint",
        },
        results: R {
            results: "*AnyConstraint",
        },
        regions: R {
            body: Region {
                kind: Nodes,
            }
        },
        interfaces: [ExitScope],
        binds: Theta {
            carried: inits ~ body.ports ~ body.results[1..n+1] ~ body.results[n+1..] ~ results,
            predicate: body.results[0],
        },
    }
}

impl ExitScope for LoopOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }
}

// A γ: the predicate is the index of the arm that runs, and a predicate past
// the last arm selects the last arm, so every predicate value picks one.
operation! {
    SwitchOp {
        name: "switch",
        dialect: "scf",
        operands: O {
            predicate: "crate::builtin::IntegerType",
            inputs: "*AnyConstraint",
        },
        results: R {
            results: "*AnyConstraint",
        },
        regions: R {
            arms: Region {
                kind: Nodes,
                variadic: true,
            }
        },
        interfaces: [ExitScope],
        binds: Gamma {
            predicate,
            forwarded: inputs ~ arms.ports,
            joined: arms.results ~ results,
        },
    }
}

impl ExitScope for SwitchOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Switch
    }
}

// A counted loop: a θ whose port 0 is the counter, starting at `lb`, tested
// against `ub` with `cmpi slt` and advanced by `step` with `addi`, and whose
// exit values are its ports. The bounds count, so they are integers or
// indices.
//
// The text elides what the shape pins: the counter's comparison, its
// increment, and the exit values. `%i, %r = scf.for %c = %lb to %ub step %s
// (%a = %init) { .. -> %next }` names the counter's final value first.
//
// The body is a block list until the converter builds the unordered form: a
// frontend recognises a counted loop while its body is still ordered, and only
// the converter sees both that body and the memory order enclosing it. The two
// states share one op, so the shape a frontend pins survives the conversion
// instead of being restated. An ordered body ends in `scf.yield`, naming what
// the next iteration carries; an unordered one names its results outright, and
// only then does the declared binding hold.
operation! {
    ForOp {
        name: "for",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            lb: "crate::builtin::Counter",
            inits: "*AnyConstraint",
            ub: "crate::builtin::Counter",
            step: "crate::builtin::Counter",
        },
        results: R {
            results: "*AnyConstraint",
        },
        regions: R {
            body: Region {
                kind: Any,
            }
        },
        interfaces: [ExitScope],
        binds: Theta {
            carried: (lb, inits) ~ body.ports ~ body.results[1..n+1] ~ body.results[n+1..] ~ results,
            predicate: body.results[0],
        },
        counted: { induction: 0, lb, ub, step },
    }
}

impl ExitScope for ForOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }
}

impl tir::Verifiable for ForOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_counter_type(context, self)?;
        verify_ordered_body(context, self)
    }
}

impl ForOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        use tir::CountedLoop;
        let context = self.0.context.upgrade();
        let body = context.get_region(Theta::body(self));
        let binding = self.carried();
        let ports: Vec<ValueId> = body.ports().iter().map(tir::Value::id).collect();
        let inits = self.operands()[binding.operands.clone()].to_vec();
        let results = body.results();
        // Unverified IR may lack the shape the text relies on; print it whole.
        if ports.is_empty()
            || inits.is_empty()
            || (body.is_nodes() && results.len() < binding.continue_.end)
        {
            return generic_print(fmt, &context, self);
        }

        tir::region_format::print_result_prefix(fmt, &self.0)?;
        fmt.write(format!(
            "scf.for %{} = %{} to %{} step %{}",
            ports[0].number(),
            inits[0].number(),
            self.upper_bound().number(),
            self.step().number()
        ))?;
        binding::print_port_bindings(fmt, &ports[1..], &inits[1..])?;
        if !body.is_nodes() {
            return tir::region_format::print_op_region(fmt, &context, self, 0);
        }

        let shown = &results[binding.continue_.start + 1..binding.continue_.end];
        let hidden: Vec<tir::OpId> = [results[0], results[binding.continue_.start]]
            .iter()
            .filter(|&&value| !context.is_used(value) && !shown.contains(&value))
            .filter_map(|&value| context.get_value(value).defining_op())
            .collect();
        tir::region_format::print_nodes_region_with(fmt, &context, &body, &hidden, shown)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
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
        if context.get_region(body).is_nodes() {
            materialize_counted_shape(context, body, counter.id(), ub, step, &bound)
                .map_err(|error| (parser.span(), error))?;
        }

        let mut result_types = vec![counter_type];
        result_types.extend(bound.ports.iter().map(tir::Value::ty));
        let builder = ForOpBuilder::new(context)
            .lb(lb)
            .inits(bound.inits)
            .ub(ub)
            .step(step)
            .body(body)
            .result_types(result_types);
        Ok(Box::new(builder.build()))
    }
}

/// The op line and body of a r#for that lacks the shape its own syntax
/// elides, printed as the generic theta so nothing is lost.
fn generic_print(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    op: &ForOp,
) -> Result<(), std::fmt::Error> {
    tir::region_format::print_result_prefix(fmt, &op.0)?;
    fmt.write("scf.for")?;
    tir::region_format::print_region(fmt, context, &context.get_region(Theta::body(op)))
}

/// Put back what the text leaves out: the comparison and increment the shape
/// pins, unless the body already spells them, and the exit values, which are
/// the ports.
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
        op.is::<crate::builtin::CmpIOp>()
            && op.attr("predicate")
                == Some(tir::attributes::AttributeValue::Predicate(Predicate::Slt))
            && op.operands().as_slice() == [counter, ub]
    })
    .unwrap_or_else(|| {
        let compare = CmpIOpBuilder::new(context)
            .lhs(counter)
            .rhs(ub)
            .predicate(Predicate::Slt)
            .result_type(IntegerType::new(context, 1))
            .build();
        context.add(body, compare.id());
        compare.result()
    });
    let advance = existing(&|op| {
        op.is::<crate::builtin::AddIOp>() && op.operands().as_slice() == [counter, step]
    })
    .unwrap_or_else(|| {
        let advance = AddIOpBuilder::new(context)
            .lhs(counter)
            .rhs(step)
            .result_type(context.get_value(counter).ty())
            .build();
        context.add(body, advance.id());
        advance.result()
    });

    let mut results = vec![compare, advance];
    results.extend(written);
    results.push(counter);
    results.extend(bound.ports.iter().map(tir::Value::id));
    context.set_region_results(body, results);
    Ok(())
}

/// A counted loop counts through one type: whatever `!index` or integer width
/// the lower bound names, the upper bound and step name too. Mixing widths
/// would leave the counter's arithmetic — and so its trip count — undefined.
fn verify_counter_type(context: &Context, op: &ForOp) -> Result<(), Error> {
    use tir::CountedLoop;
    let expected = context.get_value(op.lower_bound()).ty();
    for (bound, name) in [(op.upper_bound(), "upper bound"), (op.step(), "step")] {
        if context.get_value(bound).ty() != expected {
            return Err(Error::VerificationError(format!(
                "scf.for {name} counts through a different type than its lower bound"
            )));
        }
    }
    Ok(())
}

/// A loop whose body is still a block list carries what its ports say: the
/// counter enters as port 0 and every other port takes an init, and the body's
/// `scf.yield` names the next value of each of those, in the same types. The
/// declared binding says all this once the body is unordered; until then it is
/// the shape a frontend pinned, and nothing else checks it.
fn verify_ordered_body(context: &Context, op: &ForOp) -> Result<(), Error> {
    let body = context.get_region(Theta::body(op));
    if body.is_nodes() {
        return Ok(());
    }
    let ports = body.ports();
    let binding = op.carried();
    let inits = &op.operands()[binding.operands.clone()];
    let fail = |message: String| Err(Error::VerificationError(message));
    if ports.len() != inits.len() {
        return fail(format!(
            "scf.for takes {} initial values but its body carries {} ports",
            inits.len(),
            ports.len()
        ));
    }
    for (index, (port, &init)) in ports.iter().zip(inits).enumerate() {
        if port.ty() != context.get_value(init).ty() {
            return fail(format!("scf.for port {index} and its init differ in type"));
        }
    }
    let results = op.value_results();
    if results.len() != ports.len() {
        return fail(format!(
            "scf.for carries {} ports but has {} results",
            ports.len(),
            results.len()
        ));
    }
    let Some(entry) = body.block_ids().first().map(|&id| context.get_block(id)) else {
        return fail("scf.for has an empty body".into());
    };
    let Some(latch) = entry.op_ids().last().map(|&id| context.get_op(id)) else {
        return fail("scf.for body has no terminator".into());
    };
    if !latch.is::<crate::scf::YieldOp>() {
        return fail("an ordered scf.for body must end in scf.yield".into());
    }
    // The counter's next value is what the shape pins, so the yield names one
    // value per port past it.
    let yielded = latch.value_operands();
    if yielded.len() + 1 != ports.len() {
        return fail(format!(
            "scf.for carries {} values past its counter but its body yields {}",
            ports.len() - 1,
            yielded.len()
        ));
    }
    for (index, (port, &value)) in ports[1..].iter().zip(&yielded).enumerate() {
        if port.ty() != context.get_value(value).ty() {
            return fail(format!(
                "scf.for port {} and the value its body yields differ in type",
                index + 1
            ));
        }
    }
    Ok(())
}
