use std::ops::Range;

use crate::analysis::destructure::{Part, Schedule};
use crate::builtin::{IntegerType, ModuleOp, constant};
use crate::cfg::{BranchOpBuilder, CondBranchOpBuilder, Successor, SwitchOpBuilder};
use crate::func::{FuncOp, ReturnOpBuilder};
use crate::{
    BlockHandle, Context, Gamma, OpHandle, OpId, OperationRef, Pass, PassError, RegionHandle,
    Theta, TypeId, Value, ValueId,
};

/// Lower a function's graph and structured control flow to CFG blocks.
#[derive(Clone, Default)]
pub struct DestructurePass;

impl DestructurePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(DestructurePass, "destructure");

impl Pass for DestructurePass {
    fn name(&self) -> &'static str {
        "destructure"
    }

    fn run(&mut self, root: OpId, context: &Context) -> Result<(), PassError> {
        let op = context.get_op(root);
        if op.is::<FuncOp>() {
            return lower_function(context, &op);
        }
        if op.is::<ModuleOp>() {
            for id in context.subtree_ops(&[root]) {
                if context.has_operation(id) {
                    let op = context.get_op(id);
                    if op.is::<FuncOp>() {
                        lower_function(context, &op)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn lower_function(context: &Context, op: &OpHandle) -> Result<(), PassError> {
    let Some(&body) = op.regions().first() else {
        return Ok(());
    };
    let region = context.get_region(body);
    let blocks = region.block_ids();
    let graph = region.is_nodes();
    if !graph
        && !blocks.iter().any(|&block| {
            context
                .get_block(block)
                .op_ids()
                .into_iter()
                .any(|op| Control::of(&context.get_op(op)).is_some())
        })
    {
        return Ok(());
    }

    let schedule = Schedule::load(context, op.id)?;
    let emit = Emit {
        context,
        region,
        schedule: &schedule,
    };
    if graph {
        let entry = emit.block(emit.region.ports());
        let end = emit.ops(entry, schedule.ops(body, Part::Yield), &[])?;
        let mut ret = ReturnOpBuilder::new(context)
            .at(end.end())
            .state_operands(emit.region.state_results());
        for value in emit.region.value_results() {
            ret = ret.value(value);
        }
        ret.build();
        context.finish_region_as_cfg(body);
    } else {
        for block in blocks {
            emit.ordered(context.get_block(block))?;
        }
    }
    Ok(())
}

enum Control {
    Gamma(Box<dyn Gamma>),
    Theta(Box<dyn Theta>),
}

impl Control {
    fn of(op: &OpHandle) -> Option<Self> {
        if let Some(gamma) = op.clone().as_interface::<dyn Gamma>() {
            return Some(Self::Gamma(gamma));
        }
        op.clone().as_interface::<dyn Theta>().map(Self::Theta)
    }
}

type Arguments<'a> = &'a [(ValueId, ValueId)];

fn argument(value: ValueId, arguments: Arguments<'_>) -> ValueId {
    arguments
        .iter()
        .rev()
        .find_map(|&(from, to)| (from == value).then_some(to))
        .unwrap_or(value)
}

struct CaptureCache<'a> {
    operations: &'a [OpId],
    valid: ValueId,
    initialized: ValueId,
    payloads: Vec<(ValueId, ValueId)>,
}

struct Emit<'a> {
    context: &'a Context,
    region: RegionHandle,
    schedule: &'a Schedule,
}

impl Emit<'_> {
    fn block(&self, args: Vec<Value>) -> BlockHandle {
        let block = self.context.create_block(args);
        self.region.add_block(block.id());
        block
    }

    fn ordered(&self, mut block: BlockHandle) -> Result<(), PassError> {
        loop {
            let control = block
                .op_ids()
                .into_iter()
                .enumerate()
                .find_map(|(pos, id)| {
                    let op = self.context.get_op(id);
                    Control::of(&op).map(|control| (pos, op, control))
                });
            let Some((position, op, control)) = control else {
                return Ok(());
            };
            let join = self.context.split_block(block.id(), position + 1);
            self.control(&block, &join, op, control, &[])?;
            block = join;
        }
    }

    fn ops<'a>(
        &self,
        mut block: BlockHandle,
        operations: impl IntoIterator<Item = &'a OpId>,
        arguments: Arguments<'_>,
    ) -> Result<BlockHandle, PassError> {
        for &id in operations {
            let op = self.context.get_op(id);
            if let Some(control) = Control::of(&op) {
                let join = self.context.create_block(Vec::new());
                self.control(&block, &join, op, control, arguments)?;
                block = join;
            } else {
                self.rewrite_captures(&op, arguments);
                self.context.insert(block.end(), id);
            }
        }
        Ok(block)
    }

    fn control(
        &self,
        block: &BlockHandle,
        join: &BlockHandle,
        op: OpHandle,
        control: Control,
        arguments: Arguments<'_>,
    ) -> Result<(), PassError> {
        match control {
            Control::Gamma(gamma) => self.gamma(block, join, &op, gamma.as_ref(), arguments)?,
            Control::Theta(theta) => self.theta(block, join, &op, theta.as_ref(), arguments)?,
        }
        // Keep the same SSA IDs so operands and graph yields that name the
        // results still name them after the control operation is erased.
        for result in self.used_results(op.id, 0, &op.results()) {
            self.context.adopt_block_argument(join.id(), result);
        }
        self.region.add_block(join.id());
        self.context.erase_op(&OperationRef::new(op))
    }

    fn used_results<'a>(
        &'a self,
        op: OpId,
        first_slot: usize,
        values: &'a [ValueId],
    ) -> impl Iterator<Item = ValueId> + 'a {
        values
            .iter()
            .enumerate()
            .filter_map(move |(offset, &value)| {
                self.schedule
                    .result_used(op, first_slot + offset)
                    .then_some(value)
            })
    }

    fn used_ports(
        &self,
        region: &RegionHandle,
        slots: Range<usize>,
    ) -> impl Iterator<Item = (usize, Value)> + '_ {
        let id = region.id();
        let first = slots.start;
        region
            .ports()
            .into_iter()
            .skip(first)
            .take(slots.len())
            .enumerate()
            .filter(move |(offset, _)| self.schedule.port_used(id, first + offset))
    }

    fn gamma(
        &self,
        block: &BlockHandle,
        join: &BlockHandle,
        op: &OpHandle,
        gamma: &dyn Gamma,
        arguments: Arguments<'_>,
    ) -> Result<(), PassError> {
        let binding = gamma.binding();
        let operands = op.operands();
        let input = &operands[binding.operands];
        let mut targets = Vec::new();
        for id in gamma.arms() {
            let region = self.context.get_region(id);
            let (ports, incoming) = self
                .used_ports(&region, binding.ports.clone())
                .map(|(offset, port)| (port, argument(input[offset], arguments)))
                .unzip();
            let entry = self.block(ports);
            targets.push((entry.id(), incoming));
            let end = self.ops(entry, self.schedule.ops(id, Part::Yield), arguments)?;
            self.jump(
                &end,
                join,
                self.used_results(
                    op.id,
                    binding.results.start,
                    &region.results()[binding.exit.clone()],
                ),
                arguments,
            );
        }
        let default = targets
            .pop()
            .ok_or_else(|| PassError::InvalidRuleSet("Gamma has no default arm".into()))?;
        SwitchOpBuilder::new(self.context)
            .at(block.end())
            .selector(argument(gamma.predicate(), arguments))
            .successors(gamma.cases().into_iter().zip(targets).collect(), default)
            .build();
        Ok(())
    }

    fn theta(
        &self,
        block: &BlockHandle,
        join: &BlockHandle,
        op: &OpHandle,
        theta: &dyn Theta,
        arguments: Arguments<'_>,
    ) -> Result<(), PassError> {
        let binding = theta.binding();
        let id = theta.body();
        let body = self.context.get_region(id);
        let predicate = theta.predicate();
        let results = body.results();
        let captures = self.schedule.captures(op.id);
        let mut ports = Vec::new();
        let mut inputs = Vec::new();
        let cache = (!captures.is_empty()).then(|| {
            let flag_ty = IntegerType::new(self.context, 1);
            let valid = self.context.create_value(flag_ty, None);
            let flag = valid.id();
            ports.push(valid);
            inputs.push(self.integer(block, 0, flag_ty));

            let mut payloads = Vec::new();
            for &capture in captures {
                let results = self.context.get_op(capture).results();
                for result in self.used_results(capture, 0, &results) {
                    let ty = self.context.get_value(result).ty();
                    let payload = self.context.create_value(ty, None);
                    payloads.push((result, payload.id()));
                    ports.push(payload);
                    inputs.push(self.integer(block, 0, ty));
                }
            }
            CaptureCache {
                operations: captures,
                valid: flag,
                initialized: self.integer(block, 1, flag_ty),
                payloads,
            }
        });
        // Prefix the cache arguments so the original state arguments stay last.
        let operands = op.operands();
        let initial = &operands[binding.operands];
        let continued = &results[binding.continue_];
        let mut feedback = Vec::new();
        for (offset, port) in self.used_ports(&body, binding.ports) {
            ports.push(port);
            inputs.push(initial[offset]);
            feedback.push(continued[offset]);
        }
        let header = self.block(ports);
        self.jump(block, &header, inputs, arguments);

        let test = self.ops(header.clone(), self.schedule.ops(id, Part::Head), arguments)?;
        let operations = self
            .schedule
            .replays(op.id)
            .iter()
            .chain(self.schedule.ops(id, Part::Feedback));
        let live = self.schedule.state_in(id, Part::Feedback);
        let yes = match cache {
            Some(cache) => {
                self.cached_feedback(operations, live, &header, &feedback, arguments, &cache)?
            }
            None => self.outcome(operations, live, &header, feedback, arguments)?,
        };
        let no = self.outcome(
            self.schedule.ops(id, Part::Exit),
            self.schedule.state_in(id, Part::Exit),
            join,
            self.used_results(op.id, binding.results.start, &results[binding.exit]),
            arguments,
        )?;
        CondBranchOpBuilder::new(self.context)
            .at(test.end())
            .condition(argument(predicate, arguments))
            .true_dest(yes.0)
            .true_args(yes.1)
            .false_dest(no.0)
            .false_args(no.1)
            .build();
        Ok(())
    }

    fn integer(&self, block: &BlockHandle, value: i64, ty: TypeId) -> ValueId {
        constant(self.context, value, ty)
            .at(block.end())
            .build()
            .result()
    }

    fn cached_feedback<'a>(
        &self,
        operations: impl IntoIterator<Item = &'a OpId>,
        live: &[ValueId],
        header: &BlockHandle,
        values: &[ValueId],
        inherited: Arguments<'_>,
        cache: &CaptureCache<'_>,
    ) -> Result<Successor, PassError> {
        let check = self.block(Vec::new());
        let (check_arguments, incoming) = self.thread_state(&check, live, inherited);
        let miss = self.block(Vec::new());
        let (miss_arguments, miss_states) = self.thread_state(&miss, live, &check_arguments);
        let feedback = self.block(Vec::new());

        let mut hit_args = Vec::new();
        let mut materialized = Vec::new();
        let mut aliases = check_arguments;
        for &(result, payload) in &cache.payloads {
            let ty = self.context.get_value(result).ty();
            let arg = self.context.append_block_argument(feedback.id(), ty).id();
            aliases.push((result, arg));
            hit_args.push(payload);
            materialized.push(result);
        }
        let (feedback_arguments, hit_states) = self.thread_state(&feedback, live, &aliases);
        hit_args.extend(hit_states);
        CondBranchOpBuilder::new(self.context)
            .at(check.end())
            .condition(cache.valid)
            .true_dest(feedback.id())
            .true_args(hit_args)
            .false_dest(miss.id())
            .false_args(miss_states)
            .build();

        let miss_end = self.ops(miss, cache.operations, &miss_arguments)?;
        let mut backedge = vec![cache.initialized];
        backedge.extend_from_slice(&materialized);
        backedge.extend_from_slice(values);
        materialized.extend_from_slice(live);
        self.jump(&miss_end, &feedback, materialized, &miss_arguments);

        let end = self.ops(feedback, operations, &feedback_arguments)?;
        self.jump(&end, header, backedge, &feedback_arguments);
        Ok((check.id(), incoming))
    }

    fn outcome<'a>(
        &self,
        operations: impl IntoIterator<Item = &'a OpId>,
        live: &[ValueId],
        dest: &BlockHandle,
        values: impl IntoIterator<Item = ValueId>,
        inherited: Arguments<'_>,
    ) -> Result<Successor, PassError> {
        let mut operations = operations.into_iter().peekable();
        if operations.peek().is_none() {
            let args = values
                .into_iter()
                .map(|value| argument(value, inherited))
                .collect();
            return Ok((dest.id(), args));
        }

        let entry = self.block(Vec::new());
        let (arguments, incoming) = self.thread_state(&entry, live, inherited);
        let start = entry.id();
        let end = self.ops(entry, operations, &arguments)?;
        self.jump(&end, dest, values, &arguments);
        Ok((start, incoming))
    }

    fn thread_state(
        &self,
        entry: &BlockHandle,
        live: &[ValueId],
        inherited: Arguments<'_>,
    ) -> (Vec<(ValueId, ValueId)>, Vec<ValueId>) {
        let mut arguments = inherited.to_vec();
        let mut incoming = Vec::new();
        for &value in live {
            let input = argument(value, inherited);
            let ty = self.context.get_value(input).ty();
            let arg = self.context.append_block_argument(entry.id(), ty).id();
            arguments.push((value, arg));
            incoming.push(input);
        }
        (arguments, incoming)
    }

    fn jump(
        &self,
        from: &BlockHandle,
        to: &BlockHandle,
        values: impl IntoIterator<Item = ValueId>,
        arguments: Arguments<'_>,
    ) {
        BranchOpBuilder::new(self.context)
            .at(from.end())
            .dest(to.id())
            .dest_args(
                values
                    .into_iter()
                    .map(|value| argument(value, arguments))
                    .collect(),
            )
            .build();
    }

    fn rewrite_captures(&self, op: &OpHandle, arguments: Arguments<'_>) {
        if arguments.is_empty() {
            return;
        }
        let readers = if op.regions().is_empty() {
            vec![op.id]
        } else {
            self.context.subtree_ops(&[op.id])
        };
        for id in readers {
            let reader = self.context.get_op(id);
            for (slot, &old) in reader.operands().iter().enumerate() {
                let new = argument(old, arguments);
                if old != new {
                    self.context.set_op_operand(id, slot, new);
                }
            }
            for region in reader.regions() {
                let yields = self.context.get_region(region).results();
                let rewritten: Vec<_> = yields.iter().map(|&v| argument(v, arguments)).collect();
                if yields != rewritten {
                    self.context.set_region_results(region, rewritten);
                }
            }
        }
    }
}
