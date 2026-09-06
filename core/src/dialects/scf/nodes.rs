//! Structured control flow over unordered regions: a loop and a switch whose
//! bindings are declared, and a counted loop that pins a loop's shape.
//!
//! `scf.for` and `scf.switch` are the successors of `scf.for` and
//! `scf.switch`; they take those names once the pipeline produces them.

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
// exit values are its ports. The bounds are integers because those two ops
// are.
//
// The text elides what the shape pins: the counter's comparison, its
// increment, and the exit values. `%i, %r = scf.for %c = %lb to %ub step %s
// (%a = %init) { .. -> %next }` names the counter's final value first.
operation! {
    ForOp {
        name: "for",
        dialect: "scf",
        format: "custom",
        operands: O {
            lb: "crate::builtin::IntegerType",
            inits: "*AnyConstraint",
            ub: "crate::builtin::IntegerType",
            step: "crate::builtin::IntegerType",
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

impl ForOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        use tir::CountedLoop;
        let context = self.0.context.upgrade();
        let body = context.get_region(Theta::body(self));
        let binding = self.carried();
        let ports: Vec<ValueId> = body.value_arguments().iter().map(tir::Value::id).collect();
        let inits = self.value_operands()[binding.operands.clone()].to_vec();
        let results = body.value_results();
        // Unverified IR may lack the shape the text relies on; print it whole.
        if !body.is_nodes()
            || ports.is_empty()
            || inits.is_empty()
            || results.len() < binding.continue_.end
        {
            return generic_print(fmt, &context, self);
        }

        tir::dependency::print_result_prefix(fmt, &self.0)?;
        fmt.write(format!(
            "scf.for %{} = %{} to %{} step %{}",
            ports[0].number(),
            inits[0].number(),
            self.upper_bound().number(),
            self.step().number()
        ))?;
        let dep_ports: Vec<ValueId> = body.dep_arguments().iter().map(tir::Value::id).collect();
        binding::print_port_bindings(
            fmt,
            &ports[1..],
            &inits[1..],
            &dep_ports,
            &self.dep_operands(),
        )?;

        let shown = &results[binding.continue_.start + 1..binding.continue_.end];
        let hidden: Vec<tir::OpId> = [results[0], results[binding.continue_.start]]
            .iter()
            .filter(|&&value| !context.is_used(value) && !shown.contains(&value))
            .filter_map(|&value| context.get_value(value).defining_op())
            .collect();
        let dep_results = body.dep_results();
        tir::region_format::print_nodes_region_with(
            fmt,
            &context,
            &body,
            &hidden,
            shown,
            &dep_results[..dep_results.len() / 2],
        )
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
        let body = parser
            .parse_region_with_entry_args_and_deps(context, ports, bound.dep_ports.clone())?
            .id();
        if context.get_region(body).is_nodes() {
            materialize_counted_shape(context, body, counter.id(), ub, step, &bound)
                .map_err(|error| (parser.span(), error))?;
        }

        let mut result_types = vec![counter_type];
        result_types.extend(bound.ports.iter().map(tir::Value::ty));
        let mut builder = ForOpBuilder::new(context)
            .lb(lb)
            .inits(bound.inits)
            .ub(ub)
            .step(step)
            .body(body)
            .result_types(result_types);
        for dep in bound.dep_inits {
            builder = builder.dep_operand(dep).dep_result();
        }
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
    tir::dependency::print_result_prefix(fmt, &op.0)?;
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
    let (written, written_deps) = (region.value_results(), region.dep_results());
    if written.len() != bound.ports.len() || written_deps.len() != bound.dep_ports.len() {
        return Err(Error::VerificationError(format!(
            "scf.for carries {} values and {} dependencies but its body names {} and {}",
            bound.ports.len(),
            bound.dep_ports.len(),
            written.len(),
            written_deps.len()
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
    results.extend(written_deps);
    results.extend(bound.dep_ports.iter().map(tir::Value::id));
    context.set_region_results(body, results, 2 * bound.dep_ports.len());
    Ok(())
}
