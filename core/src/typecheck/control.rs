use super::{
    Attr, Check, Failures,
    structure::{BindingRole, Control},
};
use crate::{OpHandle, RegionId, ValueId};

impl Check<'_> {
    fn group_start(&self, field: usize) -> Option<usize> {
        self.inputs.spans.get(field)?;
        Some(self.inputs.spans[..field].iter().map(|s| s.len()).sum())
    }

    fn cases(&self, width: Option<u64>, values: Attr, errors: &mut Failures) -> Option<usize> {
        let Attr::Array(mut items) = values else {
            return None;
        };
        let case = |item: &Attr| match item {
            Attr::UInt(n) if width.is_some_and(|bits| bits >= 64 || *n < 1_u64 << bits) => Some(*n),
            _ => None,
        };
        let count = items
            .iter()
            .position(|item| case(item).is_none())
            .unwrap_or(items.len());
        let valid = &mut items[..count];
        valid.sort_unstable_by_key(case);
        errors.require(
            !valid.windows(2).any(|pair| pair[0] == pair[1]),
            "duplicate gamma case",
        );
        (count == items.len()).then_some(count)
    }

    fn pins(&self, actual: ValueId, expected: ValueId) -> bool {
        if actual == expected {
            return true;
        }
        let constant = |v| {
            self.ty(v)?;
            let value = self.ctx.get_value(v);
            let producer = value.defining_op()?;
            if self.ctx.operation_kind(producer).as_deref() != Some("builtin.constant") {
                return None;
            }
            let attr: Vec<(i64,)> = self.ctx.index_rows(
                "Attribute",
                (
                    &[0, 1, 2],
                    ("operation", i64::from(producer.number()), "value"),
                ),
                &[],
                [3],
            );
            Some((value.ty(), attr.first()?.0))
        };
        constant(actual).is_some_and(|a| Some(a) == constant(expected))
    }

    fn producer(&self, value: Option<&ValueId>, name: &str) -> Option<OpHandle> {
        let v = *value?;
        self.ty(v)?;
        let producer = self.ctx.get_value(v).defining_op()?;
        (self.ctx.operation_kind(producer).as_deref() == Some(name))
            .then(|| self.ctx.get_op(producer))
    }

    fn counted(
        &self,
        control: Option<&Control>,
        groups: (usize, usize, usize),
        errors: &mut Failures,
    ) {
        let (induction, upper, step) = groups;
        let mut comparison = false;
        let mut increment = false;
        if let Some(control) = control
            && let Some(body) = control.body
        {
            let region = self.ctx.get_region(body);
            let ports = region.ports();
            let yields = region.results();
            let operands = self.op.operands();
            let operand = |g| {
                self.group_start(g)
                    .and_then(|slot| operands.get(slot))
                    .copied()
            };
            if let (Some(counter), Some(upper), Some(step)) =
                (ports.get(induction), operand(upper), operand(step))
            {
                let counter = counter.id();
                if let Some(cmp) = self.producer(yields.get(control.predicate), "builtin.cmpi") {
                    let args = cmp.operands();
                    comparison = matches!(cmp.attr("predicate"),Some(Attr::Predicate(p)) if p.name()=="slt")
                        && args.first() == Some(&counter)
                        && args.get(1).is_some_and(|v| self.pins(*v, upper));
                }
                for (role, slot, target) in self.ctx.typecheck_bindings(body) {
                    if role == BindingRole::Exit {
                        errors.require(
                            ports
                                .get(slot)
                                .is_some_and(|p| Some(&p.id()) == yields.get(target)),
                            "exit value must be the corresponding port",
                        );
                    }
                    if role == BindingRole::Feedback
                        && slot == induction
                        && let Some(add) = self.producer(yields.get(target), "builtin.addi")
                    {
                        let args = add.operands();
                        if let [a, b, ..] = &args[..] {
                            increment |= (*a == counter && self.pins(*b, step))
                                || (*b == counter && self.pins(*a, step));
                        }
                    }
                }
            }
        }
        errors.require(
            comparison,
            "predicate must be cmpi slt of the counter and the upper bound",
        );
        errors.require(increment, "must advance the counter by the step");
    }

    fn ordered(&self, errors: &mut Failures) {
        let mut shape = false;
        for &rid in &self.op.regions() {
            let Some(block) = self.ctx.region_entry(rid) else {
                continue;
            };
            let ports = self.ctx.block_arguments(block);
            let latch = self
                .ctx
                .block_ops(block)
                .last()
                .copied()
                .filter(|op| self.ctx.operation_kind(*op).as_deref() == Some("scf.yield"));
            let latch_args = latch.map(|op| self.ctx.op_operands(op));
            let carried = self.inputs.spans.get(1);
            shape |= latch_args
                .as_ref()
                .zip(carried)
                .is_some_and(|(args, inputs)| {
                    args.len() == inputs.len()
                        && ports.len() == args.len() + 1
                        && self.op.results().len() == ports.len()
                });
            for (i, param) in ports.iter().skip(1).enumerate() {
                errors.require(
                    carried.and_then(|s| s.get(i)).copied().flatten() == self.ty(param.id()),
                    "ordered_for init",
                );
                errors.require(
                    latch_args
                        .as_ref()
                        .and_then(|xs| xs.get(i))
                        .and_then(|v| self.ty(*v))
                        == self.ty(param.id()),
                    "ordered_for yield",
                );
            }
        }
        errors.require(shape, "ordered_for shape");
    }

    fn cir(&self, roles: &[&str], errors: &mut Failures) {
        for (i, r) in self.ctx.typecheck_regions(self.op.id) {
            let Some(role) = roles.get(i as usize) else {
                continue;
            };
            let mut valid_exit = false;
            for b in self.ctx.region_blocks(r) {
                let Some(&last) = self.ctx.block_ops(b).last() else {
                    continue;
                };
                let Some(name) = self.ctx.operation_kind(last) else {
                    continue;
                };
                if self
                    .ctx
                    .kind_has_interface::<dyn crate::BranchTerminator>(&name)
                {
                    continue;
                }
                let allowed = matches!(
                    (*role, name.as_str()),
                    ("condition", "cir.condition")
                        | ("step", "cir.yield")
                        | ("body", "cir.yield" | "cir.break" | "cir.continue")
                );
                errors.require(allowed, "invalid CIR region exit");
                valid_exit |= allowed;
            }
            errors.require(valid_exit, "CIR region has no valid exit");
        }
    }

    fn switch(&self, errors: &mut Failures) {
        let width = self
            .op
            .operands()
            .first()
            .and_then(|v| self.ty(*v))
            .and_then(|t| self.ctx.terms().int_width(t));
        let cases = self
            .op
            .attr("cases")
            .map_or(Some(0), |attr| self.cases(width, attr, errors));
        let counts = match self.op.attr("arg_counts") {
            Some(Attr::Array(xs)) => xs
                .iter()
                .try_fold(0usize, |sum, x| match x {
                    Attr::UInt(n) => sum.checked_add((*n).try_into().ok()?),
                    _ => None,
                })
                .map(|total| (xs.len(), total)),
            _ => None,
        };
        let dests = match self.op.attr("dests") {
            Some(Attr::Array(xs)) if xs.iter().all(|x| matches!(x, Attr::Block(_))) => {
                Some(xs.len())
            }
            _ => None,
        };
        errors.require(
            counts.is_some_and(|(edges, total)| {
                Some(edges) == dests
                    && cases.is_some_and(|count| count.checked_add(1) == Some(edges))
                    && self.op.operands().len().checked_sub(1) == Some(total)
            }),
            "switch cases and argument counts",
        );
    }

    fn binding_shape(&self, c: &Control, errors: &mut Failures) {
        let ctx = self.ctx;
        let id = self.op.id;
        let inputs = self
            .schema
            .and_then(|s| s.binding_inputs)
            .and_then(|(first, last)| self.inputs.spans.get(first..=last))
            .map(|spans| spans.iter().map(|s| s.len()).sum::<usize>());
        let outputs = self.op.results().len();
        if c.gamma {
            let arms = self.op.regions();
            let defaults: Vec<(i64,)> =
                ctx.index_rows("Arm", (&[0, 1], (i64::from(id.number()), true)), &[], [3]);
            errors.require(
                defaults
                    .iter()
                    .any(|(r,)| ctx.region_parent_op(RegionId::from_number(*r as u32)) == Some(id)),
                "gamma default arm",
            );
            for &r in &arms {
                errors.require(
                    inputs == Some(ctx.region_ports(r).len())
                        && ctx.region_results(r).len() == outputs,
                    "gamma arm arity",
                );
            }
            if let Some(cases) = self.op.attr("cases") {
                let width = self
                    .op
                    .operands()
                    .get(c.selector)
                    .and_then(|v| self.ty(*v))
                    .and_then(|t| ctx.terms().int_width(t))
                    .filter(|w| *w >= 2);
                let count = width.and_then(|_| self.cases(width, cases, errors));
                errors.require(
                    count.is_some() && count == arms.len().checked_sub(1),
                    "gamma cases",
                );
            }
        }
        if c.theta {
            let valid = c.body.is_some_and(|r: RegionId| {
                let ys = ctx.region_results(r);
                let predicate = ys
                    .get(c.predicate)
                    .and_then(|v| self.ty(*v))
                    .and_then(|t| ctx.terms().int_width(t));
                inputs == Some(outputs)
                    && ctx.region_ports(r).len() == outputs
                    && ys.len() == outputs * 2 + 1
                    && predicate == Some(1)
            });
            errors.require(valid, "theta shape");
        }
    }

    pub(super) fn control(&self, errors: &mut Failures) {
        let control = self.ctx.typecheck_control(self.op.id);
        if let Some(c) = &control {
            self.binding_shape(c, errors);
        }
        if let Some(groups) = self.schema.and_then(|s| s.counted_groups) {
            self.counted(control.as_ref(), groups, errors);
        }
        match self.kind {
            "scf.ordered_for" => self.ordered(errors),
            "cfg.switch" => self.switch(errors),
            "cir.for" => self.cir(&["condition", "step", "body"], errors),
            "cir.while" => self.cir(&["condition", "body"], errors),
            "cir.do" => self.cir(&["body", "condition"], errors),
            _ => {}
        }
    }
}
