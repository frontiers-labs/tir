//! The counted loop a frontend raises while its body is still a block list.
//!
//! `restructure-nodes` turns it into an `scf.ordered_for`. Until then it only pins the
//! shape: the counter enters as port 0 and every other port takes an init, and
//! the body's `scf.yield` names the next value of each port past the counter.

use crate as tir;
use crate::binding;
use crate::parse::common::Cursor;
use crate::{Context, CountedLoop, Error, ExitScope, ExitScopeKind, Operation, ValueId};

__cdsl_scf_OrderedForOp!();

impl ExitScope for OrderedForOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }
}

impl CountedLoop for OrderedForOp {
    fn lower_bound(&self) -> ValueId {
        self.0.value_operands()[0]
    }
    fn upper_bound(&self) -> ValueId {
        let operands = self.0.value_operands();
        operands[operands.len() - 2]
    }
    fn step(&self) -> ValueId {
        let operands = self.0.value_operands();
        operands[operands.len() - 1]
    }
    fn induction(&self) -> Option<usize> {
        Some(0)
    }
}

impl OrderedForOp {
    /// What the ports past the counter take on the first iteration: the
    /// declared inits, then the states threading has carried in.
    pub fn inits(&self) -> Vec<ValueId> {
        let operands = self.0.operands();
        operands[1..operands.len() - 2].to_vec()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.clone();
        let body = context.get_region(self.0.regions()[0]);
        let ports: Vec<ValueId> = body.ports().iter().map(tir::Value::id).collect();
        let inits = self.inits();
        tir::region_format::print_result_prefix(fmt, &self.0)?;
        fmt.write(format!(
            "scf.ordered_for %{} = %{} to %{} step %{}",
            ports[0].number(),
            self.lower_bound().number(),
            self.upper_bound().number(),
            CountedLoop::step(self).number()
        ))?;
        binding::print_port_bindings(fmt, &ports[1..], &inits)?;
        tir::region_format::print_op_region(fmt, &context, self, 0)
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
        let mut ports = vec![counter];
        ports.extend(bound.ports.iter().cloned());
        let body = parser.parse_region_with_entry_args(context, ports)?.id();

        let mut result_types = vec![counter_type];
        result_types.extend(bound.ports.iter().map(tir::Value::ty));
        let builder = OrderedForOpBuilder::new(context)
            .lb(lb)
            .inits(bound.inits)
            .ub(ub)
            .step(step)
            .body(body)
            .result_types(result_types);
        Ok(Box::new(builder.build()))
    }
}
